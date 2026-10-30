use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};
use tracing_subscriber::{
    Layer,
    layer::{Context, SubscriberExt},
    registry::LookupSpan,
};

pub type Fields = BTreeMap<String, String>;

#[derive(Clone, Default)]
pub struct Captured(Arc<Mutex<Vec<Fields>>>);

struct Values<'a>(&'a mut Fields);

impl Visit for Values<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().into(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().into(), value.to_string());
    }
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Captured {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let mut fields = Fields::new();
        event.record(&mut Values(&mut fields));
        self.0.lock().unwrap().push(fields);
    }

    fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: Context<'_, S>) {
        let span = context.span(id).unwrap();
        let mut fields = Fields::new();
        fields.insert("span_name".into(), attributes.metadata().name().to_owned());
        fields.insert(
            "span_target".into(),
            attributes.metadata().target().to_owned(),
        );
        attributes.record(&mut Values(&mut fields));
        span.extensions_mut().insert(fields);
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, context: Context<'_, S>) {
        let span = context.span(id).unwrap();
        let mut extensions = span.extensions_mut();
        let fields = extensions.get_mut::<Fields>().unwrap();
        let mut recorded = Fields::new();
        values.record(&mut Values(&mut recorded));
        fields.extend(recorded.clone());
        if recorded
            .get("status")
            .is_some_and(|status| matches!(status.as_str(), "completed" | "error" | "cancelled"))
        {
            self.0.lock().unwrap().push(fields.clone());
        }
    }
}

impl Captured {
    pub fn subscriber(
        &self,
        targets: &'static [&'static str],
    ) -> impl Subscriber + Send + Sync + 'static {
        // 只有一个采集层；全局过滤避免并发测试 registry 的 per-layer 状态相互干扰。
        tracing_subscriber::registry()
            .with(tracing_subscriber::filter::filter_fn(move |metadata| {
                targets.iter().any(|target| *target == metadata.target())
            }))
            .with(self.clone())
    }

    pub fn take(&self) -> Vec<Fields> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}
