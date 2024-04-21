//! Interaction with Python's global interpreter lock

use crate::impl_::not_send::{NotSend, NOT_SEND};
#[cfg(feature = "disable-reference-pool")]
use crate::impl_::panic::PanicTrap;
use crate::{ffi, Python};
use parking_lot::Once;
#[cfg(not(feature = "disable-reference-pool"))]
use parking_lot::{const_mutex, Mutex};
use std::cell::Cell;
#[cfg(debug_assertions)]
use std::cell::RefCell;
#[cfg(not(debug_assertions))]
use std::cell::UnsafeCell;
use std::{mem, ptr::NonNull};

static START: Once = Once::new();

cfg_if::cfg_if! {
    if #[cfg(thread_local_const_init)] {
        use std::thread_local as thread_local_const_init;
    } else {
        macro_rules! thread_local_const_init {
            ($($(#[$attr:meta])* static $name:ident: $ty:ty = const { $init:expr };)*) => (
                thread_local! { $($(#[$attr])* static $name: $ty = $init;)* }
            )
        }
    }
}

thread_local_const_init! {
    /// This is an internal counter in pyo3 monitoring whether this thread has the GIL.
    ///
    /// It will be incremented whenever a GILGuard or GILPool is created, and decremented whenever
    /// they are dropped.
    ///
    /// As a result, if this thread has the GIL, GIL_COUNT is greater than zero.
    ///
    /// Additionally, we sometimes need to prevent safe access to the GIL,
    /// e.g. when implementing `__traverse__`, which is represented by a negative value.
    static GIL_COUNT: Cell<isize> = const { Cell::new(0) };

    /// Temporarily hold objects that will be released when the GILPool drops.
    #[cfg(debug_assertions)]
    static OWNED_OBJECTS: RefCell<PyObjVec> = const { RefCell::new(Vec::new()) };
    #[cfg(not(debug_assertions))]
    static OWNED_OBJECTS: UnsafeCell<PyObjVec> = const { UnsafeCell::new(Vec::new()) };
}

const GIL_LOCKED_DURING_TRAVERSE: isize = -1;

/// Checks whether the GIL is acquired.
///
/// Note: This uses pyo3's internal count rather than PyGILState_Check for two reasons:
///  1) for performance
///  2) PyGILState_Check always returns 1 if the sub-interpreter APIs have ever been called,
///     which could lead to incorrect conclusions that the GIL is held.
#[inline(always)]
fn gil_is_acquired() -> bool {
    GIL_COUNT.try_with(|c| c.get() > 0).unwrap_or(false)
}

/// Prepares the use of Python in a free-threaded context.
///
/// If the Python interpreter is not already initialized, this function will initialize it with
/// signal handling disabled (Python will not raise the `KeyboardInterrupt` exception). Python
/// signal handling depends on the notion of a 'main thread', which must be the thread that
/// initializes the Python interpreter.
///
/// If the Python interpreter is already initialized, this function has no effect.
///
/// This function is unavailable under PyPy because PyPy cannot be embedded in Rust (or any other
/// software). Support for this is tracked on the
/// [PyPy issue tracker](https://github.com/pypy/pypy/issues/3836).
///
/// # Examples
/// ```rust
/// use pyo3::prelude::*;
///
/// # fn main() -> PyResult<()> {
/// pyo3::prepare_freethreaded_python();
/// Python::with_gil(|py| py.run_bound("print('Hello World')", None, None))
/// # }
/// ```
#[cfg(not(any(PyPy, GraalPy)))]
pub fn prepare_freethreaded_python() {
    // Protect against race conditions when Python is not yet initialized and multiple threads
    // concurrently call 'prepare_freethreaded_python()'. Note that we do not protect against
    // concurrent initialization of the Python runtime by other users of the Python C API.
    START.call_once_force(|_| unsafe {
        // Use call_once_force because if initialization panics, it's okay to try again.
        if ffi::Py_IsInitialized() == 0 {
            ffi::Py_InitializeEx(0);

            // Release the GIL.
            ffi::PyEval_SaveThread();
        }
    });
}

/// Executes the provided closure with an embedded Python interpreter.
///
/// This function initializes the Python interpreter, executes the provided closure, and then
/// finalizes the Python interpreter.
///
/// After execution all Python resources are cleaned up, and no further Python APIs can be called.
/// Because many Python modules implemented in C do not support multiple Python interpreters in a
/// single process, it is not safe to call this function more than once. (Many such modules will not
/// initialize correctly on the second run.)
///
/// # Panics
/// - If the Python interpreter is already initialized before calling this function.
///
/// # Safety
/// - This function should only ever be called once per process (usually as part of the `main`
///   function). It is also not thread-safe.
/// - No Python APIs can be used after this function has finished executing.
/// - The return value of the closure must not contain any Python value, _including_ `PyResult`.
///
/// # Examples
///
/// ```rust
/// unsafe {
///     pyo3::with_embedded_python_interpreter(|py| {
///         if let Err(e) = py.run_bound("print('Hello World')", None, None) {
///             // We must make sure to not return a `PyErr`!
///             e.print(py);
///         }
///     });
/// }
/// ```
#[cfg(not(any(PyPy, GraalPy)))]
pub unsafe fn with_embedded_python_interpreter<F, R>(f: F) -> R
where
    F: for<'p> FnOnce(Python<'p>) -> R,
{
    assert_eq!(
        ffi::Py_IsInitialized(),
        0,
        "called `with_embedded_python_interpreter` but a Python interpreter is already running."
    );

    ffi::Py_InitializeEx(0);

    // Safety: the GIL is already held because of the Py_IntializeEx call.
    #[allow(deprecated)] // TODO: remove this with the GIL Refs feature in 0.22
    let pool = GILPool::new();

    // Import the threading module - this ensures that it will associate this thread as the "main"
    // thread, which is important to avoid an `AssertionError` at finalization.
    pool.python().import_bound("threading").unwrap();

    // Execute the closure.
    let result = f(pool.python());

    // Drop the pool before finalizing.
    drop(pool);

    // Finalize the Python interpreter.
    ffi::Py_Finalize();

    result
}

/// RAII type that represents the Global Interpreter Lock acquisition.
pub(crate) struct GILGuard {
    gstate: ffi::PyGILState_STATE,
    #[allow(deprecated)] // TODO: remove this with the gil-refs feature in 0.22
    pool: mem::ManuallyDrop<GILPool>,
}

impl GILGuard {
    /// PyO3 internal API for acquiring the GIL. The public API is Python::with_gil.
    ///
    /// If the GIL was already acquired via PyO3, this returns `None`. Otherwise,
    /// the GIL will be acquired and a new `GILPool` created.
    pub(crate) fn acquire() -> Option<Self> {
        if gil_is_acquired() {
            return None;
        }

        // Maybe auto-initialize the GIL:
        //  - If auto-initialize feature set and supported, try to initialize the interpreter.
        //  - If the auto-initialize feature is set but unsupported, emit hard errors only when the
        //    extension-module feature is not activated - extension modules don't care about
        //    auto-initialize so this avoids breaking existing builds.
        //  - Otherwise, just check the GIL is initialized.
        cfg_if::cfg_if! {
            if #[cfg(all(feature = "auto-initialize", not(any(PyPy, GraalPy))))] {
                prepare_freethreaded_python();
            } else {
                // This is a "hack" to make running `cargo test` for PyO3 convenient (i.e. no need
                // to specify `--features auto-initialize` manually. Tests within the crate itself
                // all depend on the auto-initialize feature for conciseness but Cargo does not
                // provide a mechanism to specify required features for tests.
                #[cfg(not(any(PyPy, GraalPy)))]
                if option_env!("CARGO_PRIMARY_PACKAGE").is_some() {
                    prepare_freethreaded_python();
                }

                START.call_once_force(|_| unsafe {
                    // Use call_once_force because if there is a panic because the interpreter is
                    // not initialized, it's fine for the user to initialize the interpreter and
                    // retry.
                    assert_ne!(
                        ffi::Py_IsInitialized(),
                        0,
                        "The Python interpreter is not initialized and the `auto-initialize` \
                         feature is not enabled.\n\n\
                         Consider calling `pyo3::prepare_freethreaded_python()` before attempting \
                         to use Python APIs."
                    );
                });
            }
        }

        Self::acquire_unchecked()
    }

    /// Acquires the `GILGuard` without performing any state checking.
    ///
    /// This can be called in "unsafe" contexts where the normal interpreter state
    /// checking performed by `GILGuard::acquire` may fail. This includes calling
    /// as part of multi-phase interpreter initialization.
    pub(crate) fn acquire_unchecked() -> Option<Self> {
        if gil_is_acquired() {
            return None;
        }

        let gstate = unsafe { ffi::PyGILState_Ensure() }; // acquire GIL
        #[allow(deprecated)]
        let pool = unsafe { mem::ManuallyDrop::new(GILPool::new()) };

        Some(GILGuard { gstate, pool })
    }
}

/// The Drop implementation for `GILGuard` will release the GIL.
impl Drop for GILGuard {
    fn drop(&mut self) {
        unsafe {
            // Drop the objects in the pool before attempting to release the thread state
            mem::ManuallyDrop::drop(&mut self.pool);

            ffi::PyGILState_Release(self.gstate);
        }
    }
}

// Vector of PyObject
type PyObjVec = Vec<NonNull<ffi::PyObject>>;

#[cfg(not(feature = "disable-reference-pool"))]
/// Thread-safe storage for objects which were inc_ref / dec_ref while the GIL was not held.
struct ReferencePool {
    pending_decrefs: Mutex<PyObjVec>,
}

#[cfg(not(feature = "disable-reference-pool"))]
impl ReferencePool {
    const fn new() -> Self {
        Self {
            pending_decrefs: const_mutex(Vec::new()),
        }
    }

    fn register_decref(&self, obj: NonNull<ffi::PyObject>) {
        self.pending_decrefs.lock().push(obj);
    }

    fn update_counts(&self, _py: Python<'_>) {
        let mut pending_decrefs = self.pending_decrefs.lock();
        if pending_decrefs.is_empty() {
            return;
        }

        let decrefs = mem::take(&mut *pending_decrefs);
        drop(pending_decrefs);

        for ptr in decrefs {
            unsafe { ffi::Py_DECREF(ptr.as_ptr()) };
        }
    }
}

#[cfg(not(feature = "disable-reference-pool"))]
unsafe impl Sync for ReferencePool {}

#[cfg(not(feature = "disable-reference-pool"))]
static POOL: ReferencePool = ReferencePool::new();

/// A guard which can be used to temporarily release the GIL and restore on `Drop`.
pub(crate) struct SuspendGIL {
    count: isize,
    tstate: *mut ffi::PyThreadState,
}

impl SuspendGIL {
    pub(crate) unsafe fn new() -> Self {
        let count = GIL_COUNT.with(|c| c.replace(0));
        let tstate = ffi::PyEval_SaveThread();

        Self { count, tstate }
    }
}

impl Drop for SuspendGIL {
    fn drop(&mut self) {
        GIL_COUNT.with(|c| c.set(self.count));
        unsafe {
            ffi::PyEval_RestoreThread(self.tstate);

            // Update counts of PyObjects / Py that were cloned or dropped while the GIL was released.
            #[cfg(not(feature = "disable-reference-pool"))]
            POOL.update_counts(Python::assume_gil_acquired());
        }
    }
}

/// Used to lock safe access to the GIL
pub(crate) struct LockGIL {
    count: isize,
}

impl LockGIL {
    /// Lock access to the GIL while an implementation of `__traverse__` is running
    pub fn during_traverse() -> Self {
        Self::new(GIL_LOCKED_DURING_TRAVERSE)
    }

    fn new(reason: isize) -> Self {
        let count = GIL_COUNT.with(|c| c.replace(reason));

        Self { count }
    }

    #[cold]
    fn bail(current: isize) {
        match current {
            GIL_LOCKED_DURING_TRAVERSE => panic!(
                "Access to the GIL is prohibited while a __traverse__ implmentation is running."
            ),
            _ => panic!("Access to the GIL is currently prohibited."),
        }
    }
}

impl Drop for LockGIL {
    fn drop(&mut self) {
        GIL_COUNT.with(|c| c.set(self.count));
    }
}

/// A RAII pool which PyO3 uses to store owned Python references.
///
/// See the [Memory Management] chapter of the guide for more information about how PyO3 uses
/// [`GILPool`] to manage memory.

///
/// [Memory Management]: https://pyo3.rs/main/memory.html#gil-bound-memory
#[cfg_attr(
    not(feature = "gil-refs"),
    deprecated(
        since = "0.21.0",
        note = "`GILPool` has no function if PyO3's deprecated GIL Refs API is not used"
    )
)]
pub struct GILPool {
    /// Initial length of owned objects and anys.
    /// `Option` is used since TSL can be broken when `new` is called from `atexit`.
    start: Option<usize>,
    _not_send: NotSend,
}

#[allow(deprecated)]
impl GILPool {
    /// Creates a new [`GILPool`]. This function should only ever be called with the GIL held.
    ///
    /// It is recommended not to use this API directly, but instead to use [`Python::new_pool`], as
    /// that guarantees the GIL is held.
    ///
    /// # Safety
    ///
    /// As well as requiring the GIL, see the safety notes on [`Python::new_pool`].
    #[inline]
    pub unsafe fn new() -> GILPool {
        increment_gil_count();
        // Update counts of PyObjects / Py that have been cloned or dropped since last acquisition
        #[cfg(not(feature = "disable-reference-pool"))]
        POOL.update_counts(Python::assume_gil_acquired());
        GILPool {
            start: OWNED_OBJECTS
                .try_with(|owned_objects| {
                    #[cfg(debug_assertions)]
                    let len = owned_objects.borrow().len();
                    #[cfg(not(debug_assertions))]
                    // SAFETY: This is not re-entrant.
                    let len = unsafe { (*owned_objects.get()).len() };
                    len
                })
                .ok(),
            _not_send: NOT_SEND,
        }
    }

    /// Gets the Python token associated with this [`GILPool`].
    #[inline]
    pub fn python(&self) -> Python<'_> {
        unsafe { Python::assume_gil_acquired() }
    }
}

#[allow(deprecated)]
impl Drop for GILPool {
    fn drop(&mut self) {
        if let Some(start) = self.start {
            let owned_objects = OWNED_OBJECTS.with(|owned_objects| {
                #[cfg(debug_assertions)]
                let mut owned_objects = owned_objects.borrow_mut();
                #[cfg(not(debug_assertions))]
                // SAFETY: `OWNED_OBJECTS` is released before calling Py_DECREF,
                // or Py_DECREF may call `GILPool::drop` recursively, resulting in invalid borrowing.
                let owned_objects = unsafe { &mut *owned_objects.get() };
                if start < owned_objects.len() {
                    owned_objects.split_off(start)
                } else {
                    Vec::new()
                }
            });
            for obj in owned_objects {
                unsafe {
                    ffi::Py_DECREF(obj.as_ptr());
                }
            }
        }
        decrement_gil_count();
    }
}

/// Registers a Python object pointer inside the release pool, to have its reference count increased
/// the next time the GIL is acquired in pyo3.
///
/// If the GIL is held, the reference count will be increased immediately instead of being queued
/// for later.
///
/// # Safety
/// The object must be an owned Python reference.
#[cfg(feature = "py-clone")]
#[track_caller]
pub unsafe fn register_incref(obj: NonNull<ffi::PyObject>) {
    if gil_is_acquired() {
        ffi::Py_INCREF(obj.as_ptr())
    } else {
        panic!("Cannot clone pointer into Python heap without the GIL being held.");
    }
}

/// Registers a Python object pointer inside the release pool, to have its reference count decreased
/// the next time the GIL is acquired in pyo3.
///
/// If the GIL is held, the reference count will be decreased immediately instead of being queued
/// for later.
///
/// # Safety
/// The object must be an owned Python reference.
#[track_caller]
pub unsafe fn register_decref(obj: NonNull<ffi::PyObject>) {
    if gil_is_acquired() {
        ffi::Py_DECREF(obj.as_ptr())
    } else {
        #[cfg(not(feature = "disable-reference-pool"))]
        POOL.register_decref(obj);
        #[cfg(feature = "disable-reference-pool")]
        {
            let _trap = PanicTrap::new("Aborting the process to avoid panic-from-drop.");
            panic!("Cannot drop pointer into Python heap without the GIL being held.");
        }
    }
}

/// Registers an owned object inside the GILPool, to be released when the GILPool drops.
///
/// # Safety
/// The object must be an owned Python reference.
pub unsafe fn register_owned(_py: Python<'_>, obj: NonNull<ffi::PyObject>) {
    debug_assert!(gil_is_acquired());
    // Ignores the error in case this function called from `atexit`.
    let _ = OWNED_OBJECTS.try_with(|owned_objects| {
        #[cfg(debug_assertions)]
        owned_objects.borrow_mut().push(obj);
        #[cfg(not(debug_assertions))]
        // SAFETY: This is not re-entrant.
        unsafe {
            (*owned_objects.get()).push(obj);
        }
    });
}

/// Increments pyo3's internal GIL count - to be called whenever GILPool or GILGuard is created.
#[inline(always)]
fn increment_gil_count() {
    // Ignores the error in case this function called from `atexit`.
    let _ = GIL_COUNT.try_with(|c| {
        let current = c.get();
        if current < 0 {
            LockGIL::bail(current);
        }
        c.set(current + 1);
    });
}

/// Decrements pyo3's internal GIL count - to be called whenever GILPool or GILGuard is dropped.
#[inline(always)]
fn decrement_gil_count() {
    // Ignores the error in case this function called from `atexit`.
    let _ = GIL_COUNT.try_with(|c| {
        let current = c.get();
        debug_assert!(
            current > 0,
            "Negative GIL count detected. Please report this error to the PyO3 repo as a bug."
        );
        c.set(current - 1);
    });
}

#[cfg(test)]
mod tests {
    #[allow(deprecated)]
    use super::GILPool;
    #[cfg(not(feature = "disable-reference-pool"))]
    use super::POOL;
    use super::{gil_is_acquired, GIL_COUNT, OWNED_OBJECTS};
    use crate::types::any::PyAnyMethods;
    use crate::{ffi, gil, PyObject, Python};
    use std::ptr::NonNull;

    fn get_object(py: Python<'_>) -> PyObject {
        py.eval_bound("object()", None, None).unwrap().unbind()
    }

    fn owned_object_count() -> usize {
        #[cfg(debug_assertions)]
        let len = OWNED_OBJECTS.with(|owned_objects| owned_objects.borrow().len());
        #[cfg(not(debug_assertions))]
        let len = OWNED_OBJECTS.with(|owned_objects| unsafe { (*owned_objects.get()).len() });
        len
    }

    #[cfg(not(feature = "disable-reference-pool"))]
    fn pool_dec_refs_does_not_contain(obj: &PyObject) -> bool {
        !POOL
            .pending_decrefs
            .lock()
            .contains(&unsafe { NonNull::new_unchecked(obj.as_ptr()) })
    }

    #[cfg(all(not(feature = "disable-reference-pool"), not(target_arch = "wasm32")))]
    fn pool_dec_refs_contains(obj: &PyObject) -> bool {
        POOL.pending_decrefs
            .lock()
            .contains(&unsafe { NonNull::new_unchecked(obj.as_ptr()) })
    }

    #[test]
    #[allow(deprecated)]
    fn test_owned() {
        Python::with_gil(|py| {
            let obj = get_object(py);
            let obj_ptr = obj.as_ptr();
            // Ensure that obj does not get freed
            let _ref = obj.clone_ref(py);

            unsafe {
                {
                    let pool = py.new_pool();
                    gil::register_owned(pool.python(), NonNull::new_unchecked(obj.into_ptr()));

                    assert_eq!(owned_object_count(), 1);
                    assert_eq!(ffi::Py_REFCNT(obj_ptr), 2);
                }
                {
                    let _pool = py.new_pool();
                    assert_eq!(owned_object_count(), 0);
                    assert_eq!(ffi::Py_REFCNT(obj_ptr), 1);
                }
            }
        })
    }

    #[test]
    #[allow(deprecated)]
    fn test_owned_nested() {
        Python::with_gil(|py| {
            let obj = get_object(py);
            // Ensure that obj does not get freed
            let _ref = obj.clone_ref(py);
            let obj_ptr = obj.as_ptr();

            unsafe {
                {
                    let _pool = py.new_pool();
                    assert_eq!(owned_object_count(), 0);

                    gil::register_owned(py, NonNull::new_unchecked(obj.into_ptr()));

                    assert_eq!(owned_object_count(), 1);
                    assert_eq!(ffi::Py_REFCNT(obj_ptr), 2);
                    {
                        let _pool = py.new_pool();
                        let obj = get_object(py);
                        gil::register_owned(py, NonNull::new_unchecked(obj.into_ptr()));
                        assert_eq!(owned_object_count(), 2);
                    }
                    assert_eq!(owned_object_count(), 1);
                }
                {
                    assert_eq!(owned_object_count(), 0);
                    assert_eq!(ffi::Py_REFCNT(obj_ptr), 1);
                }
            }
        });
    }

    #[test]
    fn test_pyobject_drop_with_gil_decreases_refcnt() {
        Python::with_gil(|py| {
            let obj = get_object(py);

            // Create a reference to drop with the GIL.
            let reference = obj.clone_ref(py);

            assert_eq!(obj.get_refcnt(py), 2);
            #[cfg(not(feature = "disable-reference-pool"))]
            assert!(pool_dec_refs_does_not_contain(&obj));

            // With the GIL held, reference count will be decreased immediately.
            drop(reference);

            assert_eq!(obj.get_refcnt(py), 1);
            #[cfg(not(feature = "disable-reference-pool"))]
            assert!(pool_dec_refs_does_not_contain(&obj));
        });
    }

    #[test]
    #[cfg(all(not(feature = "disable-reference-pool"), not(target_arch = "wasm32")))] // We are building wasm Python with pthreads disabled
    fn test_pyobject_drop_without_gil_doesnt_decrease_refcnt() {
        let obj = Python::with_gil(|py| {
            let obj = get_object(py);
            // Create a reference to drop without the GIL.
            let reference = obj.clone_ref(py);

            assert_eq!(obj.get_refcnt(py), 2);
            assert!(pool_dec_refs_does_not_contain(&obj));

            // Drop reference in a separate thread which doesn't have the GIL.
            std::thread::spawn(move || drop(reference)).join().unwrap();

            // The reference count should not have changed (the GIL has always
            // been held by this thread), it is remembered to release later.
            assert_eq!(obj.get_refcnt(py), 2);
            assert!(pool_dec_refs_contains(&obj));
            obj
        });

        // Next time the GIL is acquired, the reference is released
        Python::with_gil(|py| {
            assert_eq!(obj.get_refcnt(py), 1);
            assert!(pool_dec_refs_does_not_contain(&obj));
        });
    }

    #[test]
    #[allow(deprecated)]
    fn test_gil_counts() {
        // Check with_gil and GILPool both increase counts correctly
        let get_gil_count = || GIL_COUNT.with(|c| c.get());

        assert_eq!(get_gil_count(), 0);
        Python::with_gil(|_| {
            assert_eq!(get_gil_count(), 1);

            let pool = unsafe { GILPool::new() };
            assert_eq!(get_gil_count(), 2);

            let pool2 = unsafe { GILPool::new() };
            assert_eq!(get_gil_count(), 3);

            drop(pool);
            assert_eq!(get_gil_count(), 2);

            Python::with_gil(|_| {
                // nested with_gil doesn't update gil count
                assert_eq!(get_gil_count(), 2);
            });
            assert_eq!(get_gil_count(), 2);

            drop(pool2);
            assert_eq!(get_gil_count(), 1);
        });
        assert_eq!(get_gil_count(), 0);
    }

    #[test]
    fn test_allow_threads() {
        assert!(!gil_is_acquired());

        Python::with_gil(|py| {
            assert!(gil_is_acquired());

            py.allow_threads(move || {
                assert!(!gil_is_acquired());

                Python::with_gil(|_| assert!(gil_is_acquired()));

                assert!(!gil_is_acquired());
            });

            assert!(gil_is_acquired());
        });

        assert!(!gil_is_acquired());
    }

    #[cfg(feature = "py-clone")]
    #[test]
    #[should_panic]
    fn test_allow_threads_updates_refcounts() {
        Python::with_gil(|py| {
            // Make a simple object with 1 reference
            let obj = get_object(py);
            assert!(obj.get_refcnt(py) == 1);
            // Clone the object without the GIL which should panic
            py.allow_threads(|| obj.clone());
        });
    }

    #[test]
    fn dropping_gil_does_not_invalidate_references() {
        // Acquiring GIL for the second time should be safe - see #864
        Python::with_gil(|py| {
            let obj = Python::with_gil(|_| py.eval_bound("object()", None, None).unwrap());

            // After gil2 drops, obj should still have a reference count of one
            assert_eq!(obj.get_refcnt(), 1);
        })
    }

    #[cfg(feature = "py-clone")]
    #[test]
    fn test_clone_with_gil() {
        Python::with_gil(|py| {
            let obj = get_object(py);
            let count = obj.get_refcnt(py);

            // Cloning with the GIL should increase reference count immediately
            #[allow(clippy::redundant_clone)]
            let c = obj.clone();
            assert_eq!(count + 1, c.get_refcnt(py));
        })
    }

    #[test]
    #[cfg(not(feature = "disable-reference-pool"))]
    fn test_update_counts_does_not_deadlock() {
        // update_counts can run arbitrary Python code during Py_DECREF.
        // if the locking is implemented incorrectly, it will deadlock.

        Python::with_gil(|py| {
            let obj = get_object(py);

            unsafe extern "C" fn capsule_drop(capsule: *mut ffi::PyObject) {
                // This line will implicitly call update_counts
                // -> and so cause deadlock if update_counts is not handling recursion correctly.
                #[allow(deprecated)]
                let pool = GILPool::new();

                // Rebuild obj so that it can be dropped
                PyObject::from_owned_ptr(
                    pool.python(),
                    ffi::PyCapsule_GetPointer(capsule, std::ptr::null()) as _,
                );
            }

            let ptr = obj.into_ptr();

            let capsule =
                unsafe { ffi::PyCapsule_New(ptr as _, std::ptr::null(), Some(capsule_drop)) };

            POOL.register_decref(NonNull::new(capsule).unwrap());

            // Updating the counts will call decref on the capsule, which calls capsule_drop
            POOL.update_counts(py);
        })
    }
}
