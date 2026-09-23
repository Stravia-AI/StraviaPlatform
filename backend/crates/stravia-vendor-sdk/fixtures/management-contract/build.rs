#[path = "../../build-support/messages.rs"]
mod messages;

fn main() {
    messages::generate().expect("compile management fixture messages");
}
