#[path = "../stravia-vendor-sdk/build-support/messages.rs"]
mod messages;

fn main() {
    messages::generate().expect("compile Grok messages");
}
