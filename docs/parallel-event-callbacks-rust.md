# Parallel Event Callbacks in Rust

If you want all event subscribers to execute **in parallel**, meaning they can literally run on different CPU cores at the same time, you can use Rust threads or a thread pool such as Rayon.

## 1. Using Threads

```rust
use std::thread;

#[derive(Clone)]
struct Event {
    message: String,
}

trait Subscriber: Send + Sync {
    fn handle(&self, event: Event);
}

struct SubscriberA;
struct SubscriberB;
struct SubscriberC;

impl Subscriber for SubscriberA {
    fn handle(&self, event: Event) {
        println!("A handling: {}", event.message);

        // CPU-heavy work
        let mut x = 0u64;
        for i in 0..1_000_000_000 {
            x = x.wrapping_add(i);
        }

        println!("A done: {x}");
    }
}

impl Subscriber for SubscriberB {
    fn handle(&self, event: Event) {
        println!("B handling: {}", event.message);

        let mut x = 0u64;
        for i in 0..1_000_000_000 {
            x = x.wrapping_add(i);
        }

        println!("B done: {x}");
    }
}

impl Subscriber for SubscriberC {
    fn handle(&self, event: Event) {
        println!("C handling: {}", event.message);

        let mut x = 0u64;
        for i in 0..1_000_000_000 {
            x = x.wrapping_add(i);
        }

        println!("C done: {x}");
    }
}

fn emit(event: Event, subscribers: Vec<Box<dyn Subscriber>>) {
    let mut handles = Vec::new();

    for subscriber in subscribers {
        let event = event.clone();

        let handle = thread::spawn(move || {
            subscriber.handle(event);
        });

        handles.push(handle);
    }

    // Wait for all subscribers to finish
    for handle in handles {
        handle.join().unwrap();
    }
}

fn main() {
    let subscribers: Vec<Box<dyn Subscriber>> = vec![
        Box::new(SubscriberA),
        Box::new(SubscriberB),
        Box::new(SubscriberC),
    ];

    emit(
        Event {
            message: "Hello!".into(),
        },
        subscribers,
    );
}
```

The execution can look like:

```text
                    emit(event)
                        │
              ┌─────────┼─────────┐
              ↓         ↓         ↓
           Thread A  Thread B  Thread C
              │         │         │
              ▼         ▼         ▼
            CPU 1     CPU 2     CPU 3
              │         │         │
              └─────────┼─────────┘
                        ↓
                 wait for all
```

Instead of sequential execution:

```text
A ──────────►
             B ──────────►
                          C ──────────►
```

the subscribers can execute in parallel:

```text
A ───────────────►
B ───────────────►
C ───────────────►
```

## 2. Using Rayon

Creating an OS thread for every subscriber is usually not ideal in a real application.

A thread pool is better. Rayon provides a convenient way to do this:

```rust
use rayon::prelude::*;

#[derive(Clone)]
struct Event {
    message: String,
}

trait Subscriber: Send + Sync {
    fn handle(&self, event: Event);
}

struct SubscriberA;
struct SubscriberB;
struct SubscriberC;

impl Subscriber for SubscriberA {
    fn handle(&self, event: Event) {
        println!("A handling: {}", event.message);

        let mut x = 0u64;
        for i in 0..1_000_000_000 {
            x = x.wrapping_add(i);
        }

        println!("A done: {x}");
    }
}

impl Subscriber for SubscriberB {
    fn handle(&self, event: Event) {
        println!("B handling: {}", event.message);

        let mut x = 0u64;
        for i in 0..1_000_000_000 {
            x = x.wrapping_add(i);
        }

        println!("B done: {x}");
    }
}

impl Subscriber for SubscriberC {
    fn handle(&self, event: Event) {
        println!("C handling: {}", event.message);

        let mut x = 0u64;
        for i in 0..1_000_000_000 {
            x = x.wrapping_add(i);
        }

        println!("C done: {x}");
    }
}

fn emit(event: Event, subscribers: Vec<Box<dyn Subscriber>>) {
    subscribers
        .into_par_iter()
        .for_each(|subscriber| {
            subscriber.handle(event.clone());
        });
}

fn main() {
    let subscribers: Vec<Box<dyn Subscriber>> = vec![
        Box::new(SubscriberA),
        Box::new(SubscriberB),
        Box::new(SubscriberC),
    ];

    emit(
        Event {
            message: "Hello!".into(),
        },
        subscribers,
    );
}
```

Add Rayon to `Cargo.toml`:

```toml
[dependencies]
rayon = "1"
```

## Concurrent vs. Parallel

### Sequential

```text
Event
  │
  ▼
Subscriber A ──────►
                    │
                    ▼
                Subscriber B ──────►
                                    │
                                    ▼
                                Subscriber C
```

A must finish before B starts.

### Concurrent

```text
Event
  │
  ├──► A ───────────────►
  │
  ├──► B ─────────►
  │
  └──► C ─────────────────►
```

A, B, and C are all in progress, but they may take turns on the CPU.

### Parallel

```text
Event
  │
  ├──► A ───────────────► CPU 1
  │
  ├──► B ───────────────► CPU 2
  │
  └──► C ───────────────► CPU 3
```

A, B, and C can **literally execute simultaneously on different CPU cores**.

For **CPU-heavy event callbacks**, the Rayon approach is usually preferable to creating a new OS thread for every callback.