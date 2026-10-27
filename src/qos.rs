//! macOS schedules threads by QoS class: a process started from a background shell can land on the
//! efficiency cores only. The engine's work is on the agent's critical path, so it asks for
//! user-interactive QoS (P-cores) on the main thread and every rayon worker.

/// Raise the calling thread's QoS class (no-op off macOS).
pub fn interactive() {
    #[cfg(target_os = "macos")]
    /* SAFETY: plain syscall wrapper on the current thread; no pointers involved */
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
}

/// Build the global rayon pool with interactive QoS on every worker. Call once, before any parallel work.
pub fn init_pool() {
    interactive();
    let _ = rayon::ThreadPoolBuilder::new().start_handler(|_| interactive()).build_global();
}
