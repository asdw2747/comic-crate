//! Background conversion worker.
//!
//! All PDF parsing, image decoding and ZIP writing happen on this one thread.
//! The UI communicates through `std::sync::mpsc` channels only, so even a huge
//! batch never blocks the render loop.

use comic_crate::convert;
use comic_crate::model::{ConvertOptions, ConvertOutcome};
use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

pub enum Command {
    Add {
        id: u64,
        path: PathBuf,
    },
    Start {
        ids: Vec<u64>,
        options: ConvertOptions,
        outputs: Vec<(u64, PathBuf)>,
        cancel: Arc<AtomicBool>,
    },
    Remove(u64),
    Clear,
}

pub enum Event {
    Probed {
        id: u64,
        pages: Option<usize>,
        error: Option<String>,
    },
    Started {
        id: u64,
    },
    Progress {
        id: u64,
        done: usize,
        total: usize,
    },
    Finished {
        id: u64,
        outcome: ConvertOutcome,
    },
    Failed {
        id: u64,
        error: String,
    },
    Cancelled {
        id: u64,
    },
    BatchDone {
        processed: usize,
        succeeded: usize,
        failed: usize,
        cancelled: bool,
    },
}

pub struct Worker {
    pub tx: Sender<Command>,
    pub rx: Receiver<Event>,
}

/// Spawn the long-lived worker thread.
pub fn spawn() -> Worker {
    let (command_tx, command_rx) = mpsc::channel();
    let (event_tx, event_rx) = mpsc::channel();

    thread::Builder::new()
        .name("comiccrate-worker".to_owned())
        .spawn(move || worker_loop(command_rx, event_tx))
        .expect("could not start conversion worker thread");

    Worker {
        tx: command_tx,
        rx: event_rx,
    }
}

fn worker_loop(commands: Receiver<Command>, events: Sender<Event>) {
    let mut jobs: Vec<(u64, PathBuf)> = Vec::new();

    while let Ok(command) = commands.recv() {
        match command {
            Command::Add { id, path } => {
                if jobs.iter().any(|(job_id, _)| *job_id == id) {
                    continue;
                }
                jobs.push((id, path.clone()));

                let probe_path = path.clone();
                let probed = guarded(move || convert::probe_input(&probe_path));
                let event = match probed {
                    Ok(Ok(pages)) => Event::Probed {
                        id,
                        pages: Some(pages),
                        error: None,
                    },
                    Ok(Err(error)) => Event::Probed {
                        id,
                        pages: None,
                        error: Some(error),
                    },
                    Err(error) => Event::Probed {
                        id,
                        pages: None,
                        error: Some(error),
                    },
                };
                if events.send(event).is_err() {
                    return;
                }
            }

            Command::Start {
                ids,
                options,
                outputs,
                cancel,
            } => {
                let mut processed = 0usize;
                let mut succeeded = 0usize;
                let mut failed = 0usize;
                let mut cancelled = false;

                for id in ids {
                    if cancel.load(Ordering::Relaxed) {
                        cancelled = true;
                        break;
                    }
                    let Some((_, path)) = jobs.iter().find(|(job_id, _)| *job_id == id) else {
                        continue;
                    };
                    let path = path.clone();
                    if events.send(Event::Started { id }).is_err() {
                        return;
                    }

                    let options = options.clone();
                    let job_options = {
                        let mut job_options = options;
                        if let Some((_, output)) = outputs.iter().find(|(job_id, _)| *job_id == id)
                        {
                            job_options.output_override = Some(output.clone());
                        }
                        job_options
                    };
                    let cancel_for_job = Arc::clone(&cancel);
                    let progress_events = events.clone();
                    let result = guarded(move || {
                        let progress = move |done: usize, total: usize| {
                            let _ = progress_events.send(Event::Progress { id, done, total });
                        };
                        convert::convert_input(&path, &job_options, &cancel_for_job, &progress)
                    });

                    match result {
                        Ok(Ok(outcome)) => {
                            processed += 1;
                            succeeded += 1;
                            if events.send(Event::Finished { id, outcome }).is_err() {
                                return;
                            }
                        }
                        Ok(Err(error)) => {
                            if cancel.load(Ordering::Relaxed) {
                                cancelled = true;
                                let _ = events.send(Event::Cancelled { id });
                                break;
                            }
                            processed += 1;
                            failed += 1;
                            if events.send(Event::Failed { id, error }).is_err() {
                                return;
                            }
                        }
                        Err(error) => {
                            processed += 1;
                            failed += 1;
                            if events.send(Event::Failed { id, error }).is_err() {
                                return;
                            }
                        }
                    }
                }

                if events
                    .send(Event::BatchDone {
                        processed,
                        succeeded,
                        failed,
                        cancelled,
                    })
                    .is_err()
                {
                    return;
                }
            }

            Command::Remove(id) => {
                jobs.retain(|(job_id, _)| *job_id != id);
            }
            Command::Clear => {
                jobs.clear();
            }
        }
    }
}

fn guarded<T>(function: impl FnOnce() -> T) -> Result<T, String> {
    match catch_unwind(AssertUnwindSafe(function)) {
        Ok(value) => Ok(value),
        Err(payload) => Err(format!(
            "internal error while parsing this PDF: {}",
            panic_message(payload.as_ref())
        )),
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_owned()
    }
}
