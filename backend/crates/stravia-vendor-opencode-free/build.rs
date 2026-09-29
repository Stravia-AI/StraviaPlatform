#[path = "../stravia-vendor-sdk/build-support/messages.rs"]
mod messages;

fn main() {
    messages::generate().expect("compile OpenCode Free messages");
}
