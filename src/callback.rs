//! PHP proxy for a function which remains owned by the JavaScript worker.
use crate::{engine::Engine, marshal::arguments_to_middle};
use ext_php_rs::{prelude::*, types::Zval};
use std::rc::Rc;
#[php_class]
#[php(name = "Js\\Callback")]
pub struct JsCallback {
    pub id: u64,
    realm_id: u64,
    pub engine: Rc<Engine>,
}
impl JsCallback {
    pub fn new(id: u64, engine: Rc<Engine>) -> Self {
        Self {
            id,
            realm_id: engine.realm_id(),
            engine,
        }
    }
    pub fn check_realm(&self) -> Result<(), String> {
        if self.realm_id != self.engine.realm_id()
            || (self.engine.isolated() && !self.engine.is_active())
        {
            return Err("JS callback belongs to an ended isolated eval".into());
        }
        Ok(())
    }
}
impl Drop for JsCallback {
    fn drop(&mut self) {
        if self.check_realm().is_ok() {
            self.engine.state.queue_fn_deletion(self.id)
        }
    }
}
#[php_impl]
impl JsCallback {
    pub fn __invoke(&self, args: &[&Zval]) -> PhpResult<Zval> {
        self.check_realm().map_err(PhpException::from_message)?;
        self.engine.ensure_open()?;
        let _guard = self.engine.enter().map_err(PhpException::from_message)?;
        let args =
            arguments_to_middle(args, &self.engine.state).map_err(PhpException::from_message)?;
        self.engine.callback(self.id, args)
    }
    pub fn call(&self, args: &[&Zval]) -> PhpResult<Zval> {
        self.__invoke(args)
    }
    /// Queue a call, discard its value and report failures to awaitMessages().
    /// Admission is bounded and yields through Revolt when the queue is full.
    pub fn post(&self, args: &[&Zval]) -> PhpResult<()> {
        self.check_realm().map_err(PhpException::from_message)?;
        self.engine.ensure_post_open()?;
        let args =
            arguments_to_middle(args, &self.engine.state).map_err(PhpException::from_message)?;
        self.engine.post(self.id, args)
    }
}
