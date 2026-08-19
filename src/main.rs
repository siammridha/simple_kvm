use std::thread;
use std::time::Duration;

fn main() {
    println!("simple_kvm starting");
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}
