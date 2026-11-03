//! Windows service that starts the agent in the console session.

fn main() {
    println!("rcs-supervisor {}", rcs_protocol::BUILD_ID);
}
