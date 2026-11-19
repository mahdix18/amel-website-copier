//! The sound played when a copy finishes, built into the app.

use std::io::Cursor;
use std::time::Duration;

use rodio::{Decoder, DeviceSinkBuilder, Player};

/// `assets/sounds/finish.mp3`.
const FINISH: &[u8] = include_bytes!("../../../assets/sounds/finish.mp3");

/// Plays the finish sound on the default output device, and returns once
/// it has finished. Run it off the UI thread.
pub fn play() -> Result<(), String> {
    let source = decoder()?;
    let sink =
        DeviceSinkBuilder::open_default_sink().map_err(|e| format!("no sound device: {e}"))?;
    let player = Player::connect_new(sink.mixer());
    player.append(source);
    player.sleep_until_end();
    // Lets the device drain the last samples before it closes.
    std::thread::sleep(Duration::from_millis(100));
    Ok(())
}

fn decoder() -> Result<Decoder<Cursor<&'static [u8]>>, String> {
    Decoder::try_from(Cursor::new(FINISH)).map_err(|e| format!("cannot play the finish sound: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::Source;

    #[test]
    fn the_finish_sound_decodes() {
        let sound = decoder().unwrap();
        assert_eq!(sound.sample_rate().get(), 44100);
        assert!(sound.count() > 44100, "at least a second of samples");
    }

    /// `cargo test -p mc-gui --lib plays -- --ignored`
    #[test]
    #[ignore = "makes a sound"]
    fn plays_out_loud() {
        play().unwrap();
    }
}
