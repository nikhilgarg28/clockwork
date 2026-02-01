use crate::{queue::QueueKey, task::TaskHeader};
use futures::task::AtomicWaker;
use std::any::Any;
use std::{
    fmt,
    future::Future,
    pin::Pin,
    sync::{atomic::Ordering, Arc},
    task::{Context, Poll},
};

/// Error representing a panic that occurred in a task.
/// This is `Send` so that `JoinState` can be shared across threads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanicError {
    message: String,
}

impl PanicError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
    pub fn from_panic_payload(panic_payload: Box<dyn Any + Send>) -> Self {
        // Try to extract a meaningful error message from the panic
        let message = match panic_payload.downcast::<String>() {
            Ok(msg) => format!("Task panicked: {}", msg),
            Err(payload) => match payload.downcast::<&'static str>() {
                Ok(msg) => format!("Task panicked: {}", msg),
                Err(_) => "Task panicked with unknown payload".to_string(),
            },
        };
        Self::new(message)
    }
}

impl fmt::Display for PanicError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for PanicError {}

/// Error returned from awaiting a JoinHandle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinError {
    Cancelled,
    ResultTaken,
    Panic(PanicError),
}
#[derive(Debug)]
pub struct JoinState<T> {
    done: std::sync::atomic::AtomicBool,
    // Stored exactly once by the winner; guarded by state.
    result: std::sync::Mutex<Option<Result<T, JoinError>>>,
    waker: AtomicWaker,
}

impl<T> JoinState<T> {
    pub fn new() -> Self {
        Self {
            done: std::sync::atomic::AtomicBool::new(false),
            result: std::sync::Mutex::new(None),
            waker: AtomicWaker::new(),
        }
    }

    #[inline]
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// Internal method to complete with a result. Returns true if we won.
    ///
    /// Ordering guarantees: result is stored before `done` becomes true,
    /// so when `is_done()` returns true, result is always available.
    fn try_complete(&self, result: Result<T, JoinError>) -> bool {
        let mut guard = self.result.lock().unwrap();

        // Check if already completed (someone else won)
        if guard.is_some() {
            return false;
        }

        // Store result first (while holding lock)
        *guard = Some(result);
        drop(guard);

        // Set done flag AFTER result is visible
        self.done.store(true, Ordering::Release);
        self.waker.wake();
        true
    }

    /// Attempt to complete with Ok(val). Returns true if we won.
    pub fn try_complete_ok(&self, val: T) -> bool {
        self.try_complete(Ok(val))
    }

    /// Attempt to complete with Cancelled. Returns true if we won.
    pub fn try_complete_cancelled(&self) -> bool {
        self.try_complete(Err(JoinError::Cancelled))
    }

    /// Attempt to complete with an error. Returns true if we won.
    pub fn try_complete_err(&self, err: JoinError) -> bool {
        self.try_complete(Err(err))
    }

    /// Called by JoinHandle::poll after it sees is_done().
    /// Consumes the result exactly once.
    fn take_result(&self) -> Result<T, JoinError> {
        let mut g = self.result.lock().unwrap();
        if g.is_none() {
            return Err(JoinError::ResultTaken);
        }
        g.take().unwrap()
    }
}

/// A JoinHandle that detaches on drop, and supports explicit abort().
#[derive(Clone, Debug)]
pub struct JoinHandle<T, K: QueueKey> {
    header: Arc<TaskHeader<K>>,
    join: Arc<JoinState<T>>,
}

impl<T, K: QueueKey> JoinHandle<T, K> {
    pub fn new(header: Arc<TaskHeader<K>>, join: Arc<JoinState<T>>) -> Self {
        Self { header, join }
    }
    pub fn abort(&self) {
        self.header.cancel();
        self.join.try_complete_cancelled();
        self.header.enqueue();
    }
}

impl<T, K: QueueKey> Future for JoinHandle<T, K> {
    type Output = Result<T, JoinError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.join.is_done() {
            return Poll::Ready(self.join.take_result());
        }
        self.join.waker.register(cx.waker());
        // Re-check after registering to avoid missed wake.
        if self.join.is_done() {
            return Poll::Ready(self.join.take_result());
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_complete_ok() {
        let state = JoinState::<i32>::new();
        assert!(!state.is_done());

        assert!(state.try_complete_ok(42));
        assert!(state.is_done());

        let result = state.take_result();
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn test_complete_cancelled() {
        let state = JoinState::<i32>::new();
        assert!(state.try_complete_cancelled());
        assert!(state.is_done());

        let result = state.take_result();
        assert!(matches!(result, Err(JoinError::Cancelled)));
    }

    #[test]
    fn test_complete_panic() {
        let state = JoinState::<i32>::new();
        let panic_err = PanicError::new("test panic");
        assert!(state.try_complete_err(JoinError::Panic(panic_err)));
        assert!(state.is_done());

        let result = state.take_result();
        match result {
            Err(JoinError::Panic(e)) => assert_eq!(e.message(), "test panic"),
            other => panic!("Expected Panic error, got {:?}", other),
        }
    }

    #[test]
    fn test_only_one_completer_wins() {
        let state = JoinState::<i32>::new();

        // First completer wins
        assert!(state.try_complete_ok(1));
        // All others lose
        assert!(!state.try_complete_ok(2));
        assert!(!state.try_complete_cancelled());
        assert!(!state.try_complete_err(JoinError::Cancelled));

        let result = state.take_result();
        assert_eq!(result.unwrap(), 1);
    }

    #[test]
    fn test_result_taken_on_second_take() {
        let state = JoinState::<i32>::new();
        state.try_complete_ok(42);

        // First take succeeds
        let result1 = state.take_result();
        assert_eq!(result1.unwrap(), 42);

        // Second take returns ResultTaken
        let result2 = state.take_result();
        assert!(matches!(result2, Err(JoinError::ResultTaken)));
    }

    /// Multi-threaded test for the race condition fix.
    ///
    /// The bug: old code set `done = true` BEFORE storing the result.
    /// This caused `is_done()` to return true while result was still None.
    ///
    /// The fix: store result first, then set `done = true`.
    ///
    /// This test spawns a writer and reader thread. The reader spins until
    /// `is_done()` returns true, then immediately takes the result.
    /// With the buggy code, this fails with `Err(ResultTaken)`.
    #[test]
    fn test_race_condition_result_visible_when_done() {
        for _ in 0..1000 {
            let state = Arc::new(JoinState::<i32>::new());

            let state_writer = state.clone();
            let state_reader = state.clone();

            let writer = thread::spawn(move || {
                state_writer.try_complete_ok(42);
            });

            let reader = thread::spawn(move || {
                // Spin until done
                while !state_reader.is_done() {
                    std::hint::spin_loop();
                }
                // When is_done() returns true, result MUST be available
                let result = state_reader.take_result();
                assert!(
                    result.is_ok(),
                    "Result should be Ok(42), got {:?}. Race condition bug!",
                    result
                );
                assert_eq!(result.unwrap(), 42);
            });

            writer.join().unwrap();
            reader.join().unwrap();
        }
    }
}
