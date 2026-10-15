// Copyright 2024 - 2026 Longbridge <https://longbridge.com>
// Licensed under Apache-2.0; see LICENSE-APACHE.
// Modified for Tcode client sessions and native capture.

//! On-device Apple dictation. Recognition and utterance handling adapted from
//! gpui-component 0.7.1 (Apache-2.0); capture belongs to the same client session.

use super::{SpeechError, SpeechEvent, SpeechRecognizer, SpeechSession};
use async_channel::{Receiver, Sender, unbounded};
use block2::RcBlock;
use objc2::{AnyThread as _, rc::Retained, runtime::NSObjectProtocol as _, sel};
use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
use objc2_avf_audio::AVAudioEngine;
use objc2_foundation::{NSBundle, NSError, NSLocale, NSString};
use objc2_speech::{
    SFSpeechAudioBufferRecognitionRequest, SFSpeechRecognitionResult, SFSpeechRecognitionTask,
    SFSpeechRecognizer, SFSpeechRecognizerAuthorizationStatus,
};
use std::{
    cell::RefCell,
    future::Future,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

pub struct SystemRecognizer {
    recognizer: Option<Retained<SFSpeechRecognizer>>,
}

impl SystemRecognizer {
    pub fn new(locale: &str) -> Self {
        // Asking without usage descriptions terminates the process.
        if [
            "NSSpeechRecognitionUsageDescription",
            "NSMicrophoneUsageDescription",
        ]
        .iter()
        .any(|key| {
            NSBundle::mainBundle()
                .objectForInfoDictionaryKey(&NSString::from_str(key))
                .is_none()
        }) {
            return Self { recognizer: None };
        }
        let locale =
            NSLocale::initWithLocaleIdentifier(NSLocale::alloc(), &NSString::from_str(locale));
        // SAFETY: a valid locale; nil means it has no recognizer.
        Self {
            recognizer: unsafe {
                SFSpeechRecognizer::initWithLocale(SFSpeechRecognizer::alloc(), &locale)
            },
        }
    }

    fn on_device(&self) -> Option<&Retained<SFSpeechRecognizer>> {
        // SAFETY: plain property reads on a valid recognizer.
        self.recognizer.as_ref().filter(|recognizer| unsafe {
            recognizer.isAvailable() && recognizer.supportsOnDeviceRecognition()
        })
    }
}

impl SpeechRecognizer for SystemRecognizer {
    fn is_available(&self, _: &str) -> bool {
        self.on_device().is_some() && !speech_denied()
    }

    fn start(
        &self,
        _: &str,
        sink: Sender<SpeechEvent>,
    ) -> Result<Box<dyn SpeechSession>, SpeechError> {
        let recognizer = self.on_device().ok_or(SpeechError::Unsupported)?.clone();
        let (events, receiver) = unbounded();
        // SAFETY: plain initializer and setters on a fresh on-device request.
        let request = unsafe {
            let request = SFSpeechAudioBufferRecognitionRequest::new();
            request.setShouldReportPartialResults(true);
            request.setRequiresOnDeviceRecognition(true);
            if request.respondsToSelector(sel!(setAddsPunctuation:)) {
                request.setAddsPunctuation(true);
            }
            request
        };
        let recognition = Rc::new(RefCell::new(Recognition {
            recognizer,
            request,
            task: None,
            engine: None,
            sink,
            events: events.clone(),
            finishing: false,
            done: false,
            utterance: None,
            hypothesis: String::new(),
            last_committed: None,
        }));
        // SAFETY: reading authorization never prompts.
        let status = unsafe { SFSpeechRecognizer::authorizationStatus() };
        if status == SFSpeechRecognizerAuthorizationStatus::NotDetermined {
            let handler = RcBlock::new(move |status| {
                _ = events.try_send(Event::Authorized(
                    status == SFSpeechRecognizerAuthorizationStatus::Authorized,
                ));
            });
            // SAFETY: usage descriptions were checked; callback only sends a message.
            unsafe { SFSpeechRecognizer::requestAuthorization(&handler) };
        } else {
            _ = events.try_send(Event::Authorized(
                status == SFSpeechRecognizerAuthorizationStatus::Authorized,
            ));
        }
        let task = Box::pin(run(recognition.clone(), receiver));
        Ok(Box::new(Session { recognition, task }))
    }
}

fn speech_denied() -> bool {
    // SAFETY: reading authorization never prompts.
    let status = unsafe { SFSpeechRecognizer::authorizationStatus() };
    status == SFSpeechRecognizerAuthorizationStatus::Denied
        || status == SFSpeechRecognizerAuthorizationStatus::Restricted
}

enum Event {
    Authorized(bool),
    Microphone(bool),
    Result {
        text: String,
        is_final: bool,
        ends_utterance: bool,
    },
    Error {
        domain: String,
        code: isize,
        message: String,
    },
}

async fn run(recognition: Rc<RefCell<Recognition>>, events: Receiver<Event>) {
    while let Ok(event) = events.recv().await {
        let mut recognition = recognition.borrow_mut();
        match event {
            Event::Authorized(true) => recognition.authorize_microphone(),
            Event::Authorized(false) | Event::Microphone(false) => {
                recognition.emit(SpeechEvent::PermissionDenied);
                recognition.done = true;
            }
            Event::Microphone(true) => {
                if let Err(error) = recognition.begin() {
                    recognition.emit(SpeechEvent::Error(error));
                    recognition.done = true;
                }
            }
            Event::Result {
                text,
                is_final,
                ends_utterance,
            } => recognition.on_result(text, is_final, ends_utterance),
            Event::Error {
                domain,
                code,
                message,
            } => {
                if recognition.finishing && domain == "kAFAssistantErrorDomain" && code == 1110 {
                    let hypothesis = std::mem::take(&mut recognition.hypothesis);
                    recognition.commit(&hypothesis);
                    recognition.emit(SpeechEvent::Finished);
                } else {
                    recognition.emit(SpeechEvent::Error(
                        format!("{message} ({domain} {code})").into(),
                    ));
                }
                recognition.done = true;
            }
        }
        if recognition.done {
            break;
        }
    }
}

struct Session {
    recognition: Rc<RefCell<Recognition>>,
    task: Pin<Box<dyn Future<Output = ()>>>,
}

impl SpeechSession for Session {
    fn finish(&mut self) {
        self.recognition.borrow_mut().finish();
    }
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        self.task.as_mut().poll(cx)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let mut recognition = self.recognition.borrow_mut();
        recognition.stop_capture();
        if let Some(task) = &recognition.task {
            // SAFETY: cancel a valid task; late callbacks only reach its abandoned channel.
            unsafe { task.cancel() };
        }
    }
}

struct Recognition {
    recognizer: Retained<SFSpeechRecognizer>,
    request: Retained<SFSpeechAudioBufferRecognitionRequest>,
    task: Option<Retained<SFSpeechRecognitionTask>>,
    engine: Option<Retained<AVAudioEngine>>,
    sink: Sender<SpeechEvent>,
    events: Sender<Event>,
    finishing: bool,
    done: bool,
    utterance: Option<String>,
    hypothesis: String,
    last_committed: Option<char>,
}

impl Recognition {
    fn emit(&self, event: SpeechEvent) {
        _ = self.sink.try_send(event);
    }

    fn authorize_microphone(&self) {
        // SAFETY: reading the audio media type constant.
        let Some(audio) = (unsafe { AVMediaTypeAudio }) else {
            self.emit(SpeechEvent::Unsupported);
            return;
        };
        // SAFETY: the audio media type is valid for capture authorization.
        let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(audio) };
        let events = self.events.clone();
        if status == AVAuthorizationStatus::NotDetermined {
            let handler = RcBlock::new(move |granted: objc2::runtime::Bool| {
                _ = events.try_send(Event::Microphone(granted.as_bool()));
            });
            // SAFETY: usage descriptions were checked; callback only sends a message.
            unsafe {
                AVCaptureDevice::requestAccessForMediaType_completionHandler(audio, &handler)
            };
        } else {
            _ = events.try_send(Event::Microphone(
                status == AVAuthorizationStatus::Authorized,
            ));
        }
    }

    fn begin(&mut self) -> Result<(), SpeechError> {
        if self.finishing {
            self.emit(SpeechEvent::Finished);
            self.done = true;
            return Ok(());
        }
        let events = self.events.clone();
        let handler = RcBlock::new(
            move |result: *mut SFSpeechRecognitionResult, error: *mut NSError| {
                // SAFETY: Apple passes nil or objects valid for this call.
                let (result, error) = unsafe { (result.as_ref(), error.as_ref()) };
                if let Some(result) = result {
                    // SAFETY: property reads on a valid result.
                    _ = events.try_send(unsafe {
                        Event::Result {
                            text: result.bestTranscription().formattedString().to_string(),
                            is_final: result.isFinal(),
                            ends_utterance: result.speechRecognitionMetadata().is_some(),
                        }
                    });
                }
                if let Some(error) = error {
                    _ = events.try_send(Event::Error {
                        domain: error.domain().to_string(),
                        code: error.code(),
                        message: error.localizedDescription().to_string(),
                    });
                }
            },
        );
        // SAFETY: valid request; callback forwards results to the client thread.
        self.task = Some(unsafe {
            self.recognizer
                .recognitionTaskWithRequest_resultHandler(&self.request, &handler)
        });
        // SAFETY: a fresh engine, capture authorization was granted before this call.
        unsafe {
            let engine = AVAudioEngine::new();
            let input = engine.inputNode();
            let format = input.outputFormatForBus(0);
            if format.channelCount() == 0 || format.sampleRate() == 0. {
                return Err(SpeechError::NoInputDevice);
            }
            let request = self.request.clone();
            let sink = self.sink.clone();
            let meter = RefCell::new(super::level::LevelMeter::new(format.sampleRate()));
            let tap = RcBlock::new(
                move |buffer: std::ptr::NonNull<objc2_avf_audio::AVAudioPCMBuffer>,
                      _: std::ptr::NonNull<objc2_avf_audio::AVAudioTime>| {
                    let buffer = buffer.as_ref();
                    request.appendAudioPCMBuffer(buffer);
                    let data = buffer.floatChannelData();
                    if !data.is_null() {
                        let samples = std::slice::from_raw_parts(
                            (*data).as_ptr(),
                            buffer.frameLength() as usize,
                        );
                        let mut meter = meter.borrow_mut();
                        for &sample in samples {
                            if let Some(level) = meter.push(sample) {
                                _ = sink.try_send(SpeechEvent::Level(level));
                            }
                        }
                    }
                },
            );
            input.installTapOnBus_bufferSize_format_block(
                0,
                1280,
                Some(&format),
                RcBlock::as_ptr(&tap),
            );
            self.engine = Some(engine.clone());
            engine.prepare();
            engine
                .startAndReturnError()
                .map_err(|error| SpeechError::Input(error.to_string()))?;
        }
        self.emit(SpeechEvent::Ready);
        Ok(())
    }

    fn stop_capture(&mut self) {
        if let Some(engine) = self.engine.take() {
            // SAFETY: this session installed the only tap on input bus zero.
            unsafe {
                engine.stop();
                engine.inputNode().removeTapOnBus(0);
            }
        }
    }

    fn finish(&mut self) {
        if self.finishing {
            return;
        }
        self.finishing = true;
        self.stop_capture();
        if self.task.is_some() {
            // SAFETY: the tap has stopped appending audio to this request.
            unsafe { self.request.endAudio() };
        }
    }

    /// Results carry the whole text of the request so far, except that some
    /// macOS versions start over after a pause: the result that ends an
    /// utterance has metadata, and the next one may no longer include its
    /// text. Commit that utterance as a phrase only once it is dropped.
    fn on_result(&mut self, text: String, is_final: bool, ends_utterance: bool) {
        if let Some(utterance) = self.utterance.take()
            && starts_over(&utterance, &text)
        {
            self.commit(&utterance);
        }

        if is_final {
            self.hypothesis.clear();
            self.commit(&text);
            self.emit(SpeechEvent::Finished);
            self.done = true;
            return;
        }

        if ends_utterance {
            self.utterance = Some(text.clone());
        }
        self.emit(SpeechEvent::Hypothesis(self.joined(&text)));
        self.hypothesis = text;
    }

    fn commit(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let text = self.joined(text);
        self.last_committed = text.chars().next_back();
        self.emit(SpeechEvent::Phrase(text));
    }

    /// `text` with the separator it needs after the committed phrases.
    fn joined(&self, text: &str) -> String {
        match (self.last_committed, text.chars().next()) {
            (Some(before), Some(after)) if needs_space(before, after) => format!(" {text}"),
            _ => text.to_string(),
        }
    }
}

/// Whether `text`, the result after the one that ended `utterance`, starts a new
/// utterance instead of carrying `utterance` on.
///
/// A result that carries it on may still revise its words, punctuation or case
/// ("Hello word" becomes "Hello world, how"), so an exact prefix is too strict;
/// a result that starts over shares almost nothing with it. Less than half of
/// `utterance` in common means it started over.
fn starts_over(utterance: &str, text: &str) -> bool {
    let common = utterance
        .chars()
        .zip(text.chars())
        .take_while(|(a, b)| a == b)
        .count();
    common * 2 < utterance.chars().count()
}

/// Whether two phrases ending and starting with these characters need a space
/// between them.
fn needs_space(before: char, after: char) -> bool {
    !before.is_whitespace()
        && !after.is_whitespace()
        && !matches!(after, ',' | '.' | '?' | '!' | ';' | ':' | ')')
        && !is_unspaced_script(before)
        && !is_unspaced_script(after)
}

/// Characters of scripts written without spaces between words: Chinese,
/// Japanese and their punctuation.
fn is_unspaced_script(c: char) -> bool {
    matches!(c,
        '\u{3000}'..='\u{30FF}'
        | '\u{3400}'..='\u{4DBF}'
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{FF00}'..='\u{FFEF}'
    )
}
