//! Reproduction of the OutputDone race condition and its fix.
//!
//! ## The Bug
//! When an output receives EOS and writes the trailer, it immediately emits OutputDone.
//! However, encoder threads may still be running and sending data to the channel.
//! If a new output with the same ID is registered immediately, frames can be routed
//! to the wrong output, causing FFmpeg crashes.
//!
//! ## The Fix
//! After writing trailer (on EOS), keep draining the channel until it closes.
//! The channel only closes when all senders are dropped (encoder threads exit).
//! Only then emit OutputDone.
//!
//! Run with: cargo run -p output-done-race-repro

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender};

#[derive(Debug)]
enum Event {
    Data(u32),
    VideoEOS,
    AudioEOS,
}

/// Simulates an encoder thread that sends data and then EOS
fn encoder_thread(sender: Sender<Event>, id: &str, is_video: bool, work_after_eos: Duration) {
    // Send some data
    for i in 0..5 {
        let _ = sender.send(Event::Data(i));
        thread::sleep(Duration::from_millis(10));
    }

    // Send EOS
    let eos = if is_video {
        Event::VideoEOS
    } else {
        Event::AudioEOS
    };
    let _ = sender.send(eos);

    // Simulate work that happens after EOS (flushing, cleanup, etc.)
    // This is the critical part - the encoder thread is still alive!
    println!("[{id}] EOS sent, but thread still doing cleanup work...");
    thread::sleep(work_after_eos);
    println!("[{id}] Thread fully exiting now, dropping sender");
    // sender is dropped here when function returns
}

/// BROKEN: Original implementation that breaks immediately after EOS
fn writer_thread_broken(
    receiver: Receiver<Event>,
    output_done: Arc<AtomicBool>,
    encoder_alive_when_done: Arc<AtomicBool>,
    encoder_exited: Arc<AtomicBool>,
) {
    let mut received_video_eos = false;
    let mut received_audio_eos = false;

    for event in &receiver {
        match event {
            Event::Data(i) => println!("[BROKEN writer] Received data: {i}"),
            Event::VideoEOS => {
                println!("[BROKEN writer] Received VideoEOS");
                received_video_eos = true;
            }
            Event::AudioEOS => {
                println!("[BROKEN writer] Received AudioEOS");
                received_audio_eos = true;
            }
        }

        if received_video_eos && received_audio_eos {
            println!("[BROKEN writer] Writing trailer and breaking...");
            break; // <-- BUG: Exits immediately, encoder threads may still be alive!
        }
    }

    // Check if encoder is still alive when we emit OutputDone
    if !encoder_exited.load(Ordering::SeqCst) {
        encoder_alive_when_done.store(true, Ordering::SeqCst);
    }

    println!("[BROKEN writer] Emitting OutputDone");
    output_done.store(true, Ordering::SeqCst);
}

/// FIXED: New implementation that drains until channel closes
fn writer_thread_fixed(
    receiver: Receiver<Event>,
    output_done: Arc<AtomicBool>,
    encoder_alive_when_done: Arc<AtomicBool>,
    encoder_exited: Arc<AtomicBool>,
) {
    let mut received_video_eos = false;
    let mut received_audio_eos = false;
    let mut trailer_written = false;

    for event in &receiver {
        match event {
            Event::Data(i) => {
                if !trailer_written {
                    println!("[FIXED writer] Received data: {i}");
                }
                // After trailer, silently drain
            }
            Event::VideoEOS => {
                println!("[FIXED writer] Received VideoEOS");
                received_video_eos = true;
            }
            Event::AudioEOS => {
                println!("[FIXED writer] Received AudioEOS");
                received_audio_eos = true;
            }
        }

        if !trailer_written && received_video_eos && received_audio_eos {
            println!("[FIXED writer] Writing trailer, continuing to drain...");
            trailer_written = true;
            // Don't break - keep draining until channel closes!
        }
    }
    // Loop exits only when channel is closed (all senders dropped = encoder threads exited)

    // Check if encoder is still alive when we emit OutputDone
    if !encoder_exited.load(Ordering::SeqCst) {
        encoder_alive_when_done.store(true, Ordering::SeqCst);
    }

    println!("[FIXED writer] Emitting OutputDone");
    output_done.store(true, Ordering::SeqCst);
}

fn run_test(name: &str, use_fixed: bool) -> bool {
    println!("\n============================================================");
    println!("TEST: {name}");
    println!("============================================================\n");

    let (sender, receiver) = bounded::<Event>(10);
    let output_done = Arc::new(AtomicBool::new(false));
    let encoder_alive_when_done = Arc::new(AtomicBool::new(false));
    let encoder_exited = Arc::new(AtomicBool::new(false));

    // Spawn encoder threads
    let sender_video = sender.clone();
    let encoder_exited_video = encoder_exited.clone();
    let video_encoder = thread::spawn(move || {
        encoder_thread(
            sender_video,
            "video encoder",
            true,
            Duration::from_millis(100),
        );
        encoder_exited_video.store(true, Ordering::SeqCst);
    });

    let sender_audio = sender.clone();
    let encoder_exited_audio = encoder_exited.clone();
    let audio_encoder = thread::spawn(move || {
        encoder_thread(
            sender_audio,
            "audio encoder",
            false,
            Duration::from_millis(150),
        );
        encoder_exited_audio.store(true, Ordering::SeqCst);
    });

    // Drop the main sender so channel can close when encoder threads exit
    drop(sender);

    // Spawn writer thread
    let output_done_clone = output_done.clone();
    let encoder_alive_clone = encoder_alive_when_done.clone();
    let encoder_exited_clone = encoder_exited.clone();
    let writer = if use_fixed {
        thread::spawn(move || {
            writer_thread_fixed(
                receiver,
                output_done_clone,
                encoder_alive_clone,
                encoder_exited_clone,
            )
        })
    } else {
        thread::spawn(move || {
            writer_thread_broken(
                receiver,
                output_done_clone,
                encoder_alive_clone,
                encoder_exited_clone,
            )
        })
    };

    // Wait for everything to complete
    video_encoder.join().unwrap();
    audio_encoder.join().unwrap();
    writer.join().unwrap();

    let was_encoder_alive = encoder_alive_when_done.load(Ordering::SeqCst);

    println!();
    if was_encoder_alive {
        println!("❌ RACE CONDITION: OutputDone emitted while encoder was still alive!");
        println!("   This could cause frames to be routed to wrong output.");
    } else {
        println!("✅ SAFE: OutputDone emitted only after all encoders exited.");
    }

    !was_encoder_alive // return true if test passed
}

fn main() {
    println!("OutputDone Race Condition Reproduction");
    println!("======================================\n");
    println!("This demonstrates why we need to drain the channel until close,");
    println!("not just break after receiving EOS.\n");

    let broken_passed = run_test("BROKEN (original implementation)", false);
    let fixed_passed = run_test("FIXED (drain until channel close)", true);

    println!("\n============================================================");
    println!("SUMMARY");
    println!("============================================================");
    println!(
        "Broken implementation: {}",
        if broken_passed {
            "✅ (got lucky)"
        } else {
            "❌ RACE DETECTED"
        }
    );
    println!(
        "Fixed implementation:  {}",
        if fixed_passed {
            "✅ SAFE"
        } else {
            "❌ UNEXPECTED FAILURE"
        }
    );

    if !broken_passed && fixed_passed {
        println!("\n🎉 The fix works! OutputDone is now safe.");
    }
}
