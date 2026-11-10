//! Prints the id of the default output device once a second, for checking device changes by hand.
fn main() {
    let _com = rcs_audio::ComApartment::enter().expect("COM");
    for _ in 0..240 {
        println!("{:?}", rcs_audio::default_device_id());
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
