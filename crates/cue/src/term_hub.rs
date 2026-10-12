//! The host's terminals: each program's pty, on a thread of its own (see
//! [`pty::serve`]), and the call that hears of it, all on the hub's thread.
//! They outlive the calls: a terminal hangs up when its call's cancelled,
//! as when the app closes it, or when the host ends, but not when its
//! client's gone. Another client can attach to it then (see
//! [`Start::Attach`]), and hears of it from there on instead.
//!
//! In a host process, the hub keeps each terminal's screen too, as the
//! app does, in an emulator of its own; the native core allows one thread
//! per process, so they're all on the hub's. A client that attaches gets
//! the screen first ([`Event::Screen`]), then what the program writes; and
//! while no client hears of a terminal, the hub answers the program's
//! queries. The host in the app's process has none, the core being the
//! app's: a terminal there can't be attached to again.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

use opentui::EmbeddedTerminal;

use crate::host::{Reply, Responder};
use crate::pty::{self, Control, Event, Start, Status};

/// Tells a terminal from the others on the same host, across clients.
pub type TermId = u64;

/// The hub's end for the host's other threads.
pub struct Hub {
    to_hub: Sender<ToHub>,
    next_id: AtomicU64,
    next_token: AtomicU64,
    /// Terminals running.
    count: Arc<AtomicUsize>,
}

/// Which call hears of a terminal, by a token for it unique to the host.
pub type Token = u64;

enum ToHub {
    /// Start the terminal `id` as `start` says, or attach to it, for the
    /// call `responder` answers.
    Open {
        id: TermId,
        start: Start,
        token: Token,
        responder: Responder,
        #[cfg(test)]
        config: Option<Arc<crate::config::Config>>,
    },
    Control(TermId, Control),
    /// The call `token` no longer hears of it, if it still does.
    Unwatch(TermId, Token),
    /// From a terminal's thread.
    Event(TermId, Event),
    /// A terminal's thread ended.
    Ended(TermId),
    HangUpAll,
    /// Ends the call it answers, once what came before is sent.
    After(Responder),
    /// Which terminals are running, said to this.
    List(Sender<Vec<TermId>>),
    /// The host's done: hang up on every terminal, and end once they have.
    Close,
}

impl Hub {
    /// Starts the hub's thread, keeping terminals' screens if `screens`.
    pub fn new(screens: bool) -> Hub {
        let (to_hub, inbox) = mpsc::channel();
        let count = Arc::new(AtomicUsize::new(0));
        let hub = to_hub.clone();
        let running = Arc::clone(&count);
        std::thread::spawn(move || run(inbox, hub, &running, screens));
        Hub {
            to_hub,
            next_id: AtomicU64::new(1),
            next_token: AtomicU64::new(1),
            count,
        }
    }

    /// Starts a terminal, or attaches to one, as `start` says, for the call
    /// `responder` answers: which terminal it is, and the token the call
    /// hears of it by.
    pub fn open(
        &self,
        start: Start,
        responder: Responder,
        #[cfg(test)] config: Option<Arc<crate::config::Config>>,
    ) -> (TermId, Token) {
        let id = match start {
            Start::Attach(id) => id,
            _ => self.next_id.fetch_add(1, Ordering::Relaxed),
        };
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        let _ = self.to_hub.send(ToHub::Open {
            id,
            start,
            token,
            responder,
            #[cfg(test)]
            config,
        });
        (id, token)
    }

    pub fn control(&self, id: TermId, control: Control) {
        let _ = self.to_hub.send(ToHub::Control(id, control));
    }

    /// Stops the call `token` hearing of the terminal `id`, leaving it
    /// running, unless another's attached to it since.
    pub fn unwatch(&self, id: TermId, token: Token) {
        let _ = self.to_hub.send(ToHub::Unwatch(id, token));
    }

    /// Ends the call `responder` answers once what the hub sent before is
    /// on its way: after [`Hub::unwatch`], the last of the output.
    pub fn after(&self, responder: Responder) {
        let _ = self.to_hub.send(ToHub::After(responder));
    }

    /// Hangs up on every terminal.
    pub fn hang_up_all(&self) {
        let _ = self.to_hub.send(ToHub::HangUpAll);
    }

    /// Which terminals are running, by id.
    pub fn list(&self) -> Vec<TermId> {
        let (answer, listed) = mpsc::channel();
        let _ = self.to_hub.send(ToHub::List(answer));
        listed.recv().unwrap_or_default()
    }

    /// How many terminals are running.
    pub fn count(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.to_hub.send(ToHub::Close);
    }
}

/// A terminal, as the hub has it.
struct Term {
    controller: pty::Controller,
    /// Its screen, if the hub keeps them.
    vt: Option<EmbeddedTerminal>,
    size: (u16, u16),
    /// The call that hears of it, and its token.
    watcher: Option<(Token, Responder)>,
    /// How it was last said to be doing, for a client that attaches.
    status: Status,
    exited: Option<i32>,
}

impl Term {
    /// Tells the call that hears of it, if one does and still listens.
    fn tell(&mut self, event: Event) {
        if let Some((_, responder)) = &self.watcher {
            if !responder.reply(Reply::Term(event)) {
                self.watcher = None;
            }
        }
    }
}

/// The hub's thread: takes what comes, until the host's done and every
/// terminal's gone.
fn run(inbox: Receiver<ToHub>, hub: Sender<ToHub>, count: &AtomicUsize, screens: bool) {
    let mut terms: HashMap<TermId, Term> = HashMap::new();
    let mut closing = false;
    for message in inbox {
        match message {
            ToHub::Open {
                id,
                start: Start::Attach(_),
                token,
                responder,
                ..
            } => {
                let Some(term) = terms.get_mut(&id) else {
                    let gone = "That terminal is gone.".to_string();
                    responder.reply(Reply::Term(Event::Failed(gone)));
                    continue;
                };
                // The call that heard of it before ends.
                term.watcher = Some((token, responder));
                if let Some(vt) = &mut term.vt {
                    if let Some(screen) = crate::terminal::full_snapshot(vt) {
                        let (cols, rows) = term.size;
                        term.tell(Event::Screen { screen, cols, rows });
                    }
                }
                term.tell(Event::Status(term.status.clone()));
                if let Some(raw) = term.exited {
                    term.tell(Event::Exited(raw));
                }
            }
            ToHub::Open {
                id,
                start,
                token,
                responder,
                #[cfg(test)]
                config,
            } => {
                #[cfg(test)]
                if let Some(config) = &config {
                    crate::config::set((**config).clone());
                }
                let (controller, controls) = match pty::controls() {
                    Ok(controls) => controls,
                    Err(err) => {
                        responder.reply(Reply::Term(Event::Failed(err.to_string())));
                        continue;
                    }
                };
                let size = match &start {
                    Start::Shell { cols, rows, .. } => (*cols, *rows),
                    _ => (80, 24),
                };
                // Not for a pty adopted, whose screen the app has.
                let vt = match &start {
                    Start::Shell { above, .. } if screens => {
                        let scrollback = crate::config::get().scrollback;
                        let vt = EmbeddedTerminal::new(size.0.max(1), size.1.max(1), scrollback);
                        vt.ok().map(|mut vt| {
                            let _ = vt.write(above);
                            vt.drain_responses();
                            vt
                        })
                    }
                    _ => None,
                };
                let mut term = Term {
                    controller,
                    vt,
                    size,
                    watcher: Some((token, responder)),
                    status: Status::default(),
                    exited: None,
                };
                term.tell(Event::Started(id));
                terms.insert(id, term);
                count.fetch_add(1, Ordering::Relaxed);
                let hub = hub.clone();
                std::thread::spawn(move || {
                    #[cfg(test)]
                    if let Some(config) = config {
                        crate::config::set((*config).clone());
                    }
                    pty::serve(start, controls, |event| {
                        hub.send(ToHub::Event(id, event)).is_ok()
                    });
                    let _ = hub.send(ToHub::Ended(id));
                });
            }
            ToHub::Control(id, control) => {
                let Some(term) = terms.get_mut(&id) else {
                    continue;
                };
                if let Control::Resize(cols, rows) = control {
                    term.size = (cols, rows);
                    if let Some(vt) = &mut term.vt {
                        let _ = vt.resize(cols.max(1), rows.max(1));
                    }
                }
                term.controller.send(control);
            }
            ToHub::Unwatch(id, token) => {
                if let Some(term) = terms.get_mut(&id) {
                    if term.watcher.as_ref().is_some_and(|(t, _)| *t == token) {
                        term.watcher = None;
                    }
                }
            }
            ToHub::Event(id, event) => {
                let Some(term) = terms.get_mut(&id) else {
                    continue;
                };
                let mut answers = Vec::new();
                match &event {
                    Event::Output(bytes) => {
                        if let Some(vt) = &mut term.vt {
                            let _ = vt.write(bytes);
                            answers = vt.drain_responses();
                        }
                    }
                    Event::Status(status) => term.status = status.clone(),
                    Event::Exited(raw) => term.exited = Some(*raw),
                    _ => {}
                }
                term.tell(event);
                // The client's emulator answers, while there's one: looked
                // at once it's been told, so one that went meanwhile isn't
                // counted on.
                if term.watcher.is_none() && !answers.is_empty() {
                    term.controller.send(Control::Input(answers));
                }
            }
            ToHub::Ended(id) => {
                // Its call ends with it.
                if terms.remove(&id).is_some() {
                    count.fetch_sub(1, Ordering::Relaxed);
                }
            }
            ToHub::After(responder) => drop(responder),
            ToHub::HangUpAll => {
                for term in terms.values() {
                    term.controller.send(Control::HangUp);
                }
            }
            ToHub::List(answer) => {
                let mut ids: Vec<TermId> = terms.keys().copied().collect();
                ids.sort_unstable();
                let _ = answer.send(ids);
            }
            ToHub::Close => {
                closing = true;
                for term in terms.values() {
                    term.controller.send(Control::HangUp);
                }
            }
        }
        if closing && terms.is_empty() {
            break;
        }
    }
}
