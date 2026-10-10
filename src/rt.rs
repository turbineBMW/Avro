//! One shared tokio runtime; the GTK thread hands futures to it.
use std::sync::OnceLock;
pub fn handle() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Runtime::new().expect("tokio runtime"))
}
pub fn block_on<F: std::future::Future>(f: F) -> F::Output { handle().block_on(f) }
pub fn spawn<F: std::future::Future<Output = ()> + Send + 'static>(f: F) { handle().spawn(f); }
