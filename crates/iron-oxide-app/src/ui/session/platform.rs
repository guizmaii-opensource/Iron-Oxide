//! The browser calls of the session screens: the clock and a little `localStorage`.
//!
//! Outside the browser (the server render, host tests) the clock is the system's and storage is
//! absent; nothing here is called during a render, only from event handlers and client effects.

use iron_oxide_domain::time::Timestamp;

/// The client's clock (`Date.now()` in the browser).
#[must_use]
pub fn now() -> Timestamp {
    #[cfg(feature = "web")]
    {
        // Milliseconds since the epoch fit in an i64 for the next 290 million years.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "Date.now() is a whole number of ms"
        )]
        Timestamp::from_epoch_millis(js_sys::Date::now() as i64)
    }
    #[cfg(not(feature = "web"))]
    {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis());
        Timestamp::from_epoch_millis(i64::try_from(millis).unwrap_or(i64::MAX))
    }
}

/// Reads a `localStorage` entry. `None` when absent or when storage is unavailable (private
/// mode, blocked site data).
#[must_use]
pub fn load(key: &str) -> Option<String> {
    #[cfg(feature = "web")]
    {
        storage()?.get_item(key).ok().flatten()
    }
    #[cfg(not(feature = "web"))]
    {
        let _ = key;
        None
    }
}

/// Writes a `localStorage` entry, best effort: the entries kept here are conveniences that the
/// app can do without.
pub fn store(key: &str, value: &str) {
    #[cfg(feature = "web")]
    if let Some(storage) = storage() {
        let _ = storage.set_item(key, value);
    }
    #[cfg(not(feature = "web"))]
    let _ = (key, value);
}

/// Removes a `localStorage` entry, best effort.
pub fn remove(key: &str) {
    #[cfg(feature = "web")]
    if let Some(storage) = storage() {
        let _ = storage.remove_item(key);
    }
    #[cfg(not(feature = "web"))]
    let _ = key;
}

#[cfg(feature = "web")]
fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

/// What the rest timer announces with a sound and a vibration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cue {
    /// 10 seconds left: one short beep.
    Warning,
    /// The rest is over: three beeps.
    Finished,
}

/// Unlocks sound playback. iOS only lets a page play audio after it started (or resumed) its
/// audio context inside a user gesture, so every tap of the session screens calls this,
/// synchronously, before any `await`. Silent; best effort.
pub fn unlock_audio() {
    #[cfg(feature = "web")]
    web::unlock_audio();
}

/// Keeps the audio ready while it is alive: the next tap or key anywhere on the page, and the page
/// becoming visible again, resume an audio context the browser suspended or iOS "interrupted"
/// (a lock screen, a call). Dropping it stops listening.
pub struct KeepAudioReady {
    #[cfg(feature = "web")]
    _inner: Vec<web::DocumentListener>,
}

impl KeepAudioReady {
    #[must_use]
    pub fn new() -> Self {
        Self {
            #[cfg(feature = "web")]
            _inner: ["click", "touchend", "keydown", "visibilitychange"]
                .into_iter()
                .map(|event| web::DocumentListener::new(event, web::unlock_audio))
                .collect(),
        }
    }
}

/// Plays `cue`'s beeps when `sound` is on, and vibrates when `vibration` is on and the device
/// can. Best effort: a
/// browser without Web Audio or vibration just skips that part. Beeps are only scheduled on a
/// running audio context: one that is suspended would play them late, at the next tap, so they
/// are dropped instead.
pub fn announce(cue: Cue, sound: bool, vibration: bool) {
    #[cfg(feature = "web")]
    web::announce(cue, sound, vibration);
    #[cfg(not(feature = "web"))]
    let _ = (cue, sound, vibration);
}

/// Keeps the screen on while it is alive (Screen Wake Lock), asking again each time the page
/// becomes visible (the browser releases the lock when it is hidden). Where the API is missing or
/// refused, nothing happens. Dropping it releases the lock.
pub struct ScreenAwake {
    #[cfg(feature = "web")]
    _inner: web::ScreenAwake,
}

impl ScreenAwake {
    #[must_use]
    pub fn keep() -> Self {
        Self {
            #[cfg(feature = "web")]
            _inner: web::ScreenAwake::keep(),
        }
    }
}

/// Calls a function each time the page becomes visible again (`visibilitychange`), so timers
/// recompute from the clock at once after a lock screen or a tab switch. Dropping it stops.
pub struct OnVisible {
    #[cfg(feature = "web")]
    _inner: web::OnVisible,
}

impl OnVisible {
    #[must_use]
    pub fn new(callback: impl FnMut() + 'static) -> Self {
        #[cfg(feature = "web")]
        {
            Self {
                _inner: web::OnVisible::new(callback),
            }
        }
        #[cfg(not(feature = "web"))]
        {
            let _ = callback;
            Self {}
        }
    }
}

#[cfg(feature = "web")]
mod web {
    use std::cell::RefCell;
    use std::rc::Rc;

    use wasm_bindgen::JsCast;
    use wasm_bindgen::prelude::Closure;
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{
        AudioContext, AudioContextState, Document, VisibilityState, WakeLockSentinel, WakeLockType,
    };

    use super::Cue;

    thread_local! {
        /// The page's one audio context, created on the first tap.
        static AUDIO: RefCell<Option<AudioContext>> = const { RefCell::new(None) };
    }

    pub fn unlock_audio() {
        AUDIO.with(|audio| {
            let mut audio = audio.borrow_mut();
            if audio.is_none() {
                *audio = AudioContext::new().ok();
                // A silent blip started inside the gesture is what unlocks iOS.
                if let Some(context) = audio.as_ref() {
                    let _ = beep(context, 0.0, 0.01, 0.0001);
                }
            }
            // A closed context cannot come back: start a new one.
            if audio
                .as_ref()
                .is_some_and(|context| context.state() == AudioContextState::Closed)
            {
                *audio = AudioContext::new().ok();
            }
            // Suspended, or WebKit's non-standard "interrupted" (which web-sys does not name):
            // anything but running is resumed.
            if let Some(context) = audio.as_ref()
                && context.state() != AudioContextState::Running
            {
                let _ = context.resume();
            }
        });
    }

    pub fn announce(cue: Cue, sound: bool, vibration: bool) {
        if sound {
            // After a reload no tap has unlocked the audio yet: try anyway, which works where the
            // browser allows it (not on iOS, which stays silent until the next tap).
            unlock_audio();
            AUDIO.with(|audio| {
                if let Some(context) = audio.borrow().as_ref()
                    && context.state() == AudioContextState::Running
                {
                    let beeps: &[f64] = match cue {
                        Cue::Warning => &[0.0],
                        Cue::Finished => &[0.0, 0.3, 0.6],
                    };
                    for &offset in beeps {
                        let _ = beep(context, offset, 0.18, 0.35);
                    }
                }
            });
        }
        if vibration
            && let Some(navigator) = web_sys::window().map(|window| window.navigator())
            && has(&navigator, "vibrate")
        {
            let _ = match cue {
                Cue::Warning => navigator.vibrate_with_duration(150),
                Cue::Finished => {
                    let pattern = js_sys::Array::of3(&300.into(), &150.into(), &300.into());
                    navigator.vibrate_with_pattern(&pattern)
                }
            };
        }
    }

    /// One 880 Hz beep of `length` seconds, `offset` seconds from now, at `volume`.
    fn beep(
        context: &AudioContext,
        offset: f64,
        length: f64,
        volume: f32,
    ) -> Result<(), wasm_bindgen::JsValue> {
        let oscillator = context.create_oscillator()?;
        let gain = context.create_gain()?;
        oscillator.frequency().set_value(880.0);
        oscillator.connect_with_audio_node(&gain)?;
        gain.connect_with_audio_node(&context.destination())?;
        let start = context.current_time() + offset;
        gain.gain().set_value_at_time(volume, start)?;
        // Fade out instead of cutting, which clicks.
        gain.gain()
            .exponential_ramp_to_value_at_time(0.0001, start + length)?;
        oscillator.start_with_when(start)?;
        oscillator.stop_with_when(start + length + 0.02)
    }

    fn has(target: &wasm_bindgen::JsValue, name: &str) -> bool {
        js_sys::Reflect::has(target, &name.into()).unwrap_or(false)
    }

    fn document() -> Option<Document> {
        web_sys::window()?.document()
    }

    fn visible() -> bool {
        document().is_some_and(|document| document.visibility_state() == VisibilityState::Visible)
    }

    /// A listener for `event` on the document, removed on drop.
    pub struct DocumentListener {
        event: &'static str,
        listener: Option<Closure<dyn FnMut()>>,
    }

    impl DocumentListener {
        pub fn new(event: &'static str, callback: impl FnMut() + 'static) -> Self {
            let listener = Closure::<dyn FnMut()>::new(callback);
            let added = document().is_some_and(|document| {
                document
                    .add_event_listener_with_callback(event, listener.as_ref().unchecked_ref())
                    .is_ok()
            });
            Self {
                event,
                listener: added.then_some(listener),
            }
        }
    }

    impl Drop for DocumentListener {
        fn drop(&mut self) {
            if let (Some(listener), Some(document)) = (self.listener.take(), document()) {
                let _ = document.remove_event_listener_with_callback(
                    self.event,
                    listener.as_ref().unchecked_ref(),
                );
            }
        }
    }

    pub struct OnVisible {
        listener: Option<Closure<dyn FnMut()>>,
    }

    impl OnVisible {
        pub fn new(mut callback: impl FnMut() + 'static) -> Self {
            let listener = Closure::<dyn FnMut()>::new(move || {
                if visible() {
                    callback();
                }
            });
            let added = document().is_some_and(|document| {
                document
                    .add_event_listener_with_callback(
                        "visibilitychange",
                        listener.as_ref().unchecked_ref(),
                    )
                    .is_ok()
            });
            Self {
                listener: added.then_some(listener),
            }
        }
    }

    impl Drop for OnVisible {
        fn drop(&mut self) {
            if let (Some(listener), Some(document)) = (self.listener.take(), document()) {
                let _ = document.remove_event_listener_with_callback(
                    "visibilitychange",
                    listener.as_ref().unchecked_ref(),
                );
            }
        }
    }

    /// The lock currently held, shared with the requests in flight.
    #[derive(Default)]
    struct Lock {
        sentinel: Option<WakeLockSentinel>,
        /// False once the owner is dropped: a request that lands later releases at once.
        wanted: bool,
    }

    pub struct ScreenAwake {
        lock: Rc<RefCell<Lock>>,
        _on_visible: OnVisible,
    }

    impl ScreenAwake {
        pub fn keep() -> Self {
            let lock = Rc::new(RefCell::new(Lock {
                sentinel: None,
                wanted: true,
            }));
            request(&lock);
            let again = Rc::clone(&lock);
            Self {
                lock,
                _on_visible: OnVisible::new(move || request(&again)),
            }
        }
    }

    impl Drop for ScreenAwake {
        fn drop(&mut self) {
            let mut lock = self.lock.borrow_mut();
            lock.wanted = false;
            if let Some(sentinel) = lock.sentinel.take() {
                let _ = sentinel.release();
            }
        }
    }

    /// Asks for the screen lock, unless one is held. Silent when unsupported or refused (low
    /// battery, page hidden, no permission).
    fn request(lock: &Rc<RefCell<Lock>>) {
        let held = lock
            .borrow()
            .sentinel
            .as_ref()
            .is_some_and(|sentinel| !sentinel.released());
        let Some(navigator) = web_sys::window().map(|window| window.navigator()) else {
            return;
        };
        if held || !visible() || !has(&navigator, "wakeLock") {
            return;
        }
        let promise = navigator.wake_lock().request(WakeLockType::Screen);
        let lock = Rc::clone(lock);
        wasm_bindgen_futures::spawn_local(async move {
            let Ok(value) = JsFuture::from(promise).await else {
                return;
            };
            let Ok(sentinel) = value.dyn_into::<WakeLockSentinel>() else {
                return;
            };
            let mut lock = lock.borrow_mut();
            if lock.wanted {
                lock.sentinel = Some(sentinel);
            } else {
                let _ = sentinel.release();
            }
        });
    }
}
