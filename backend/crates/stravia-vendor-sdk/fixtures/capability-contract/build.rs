#[path = "../../build-support/messages.rs"]
mod messages;

fn main() {
    messages::generate().expect("compile capability fixture messages");
}
