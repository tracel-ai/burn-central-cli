/// Trait for reporting events of type `E`.
pub trait Reporter<E>: Send + Sync {
    fn report_event(&self, event: E);
}

impl<E> Reporter<E> for () {
    fn report_event(&self, _event: E) {}
}

impl<F, E> Reporter<E> for F
where
    F: Fn(E) + Send + Sync,
{
    fn report_event(&self, event: E) {
        self(event)
    }
}

impl<E: Send> Reporter<E> for std::sync::mpsc::Sender<E> {
    fn report_event(&self, event: E) {
        let _ = self.send(event);
    }
}
