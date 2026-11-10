use super::*;
use std::sync::mpsc;

// Four retained outputs, one active result and eight buffered events bound a
// retirement wave. Only a successfully spawned worker submits a barrier, and it
// waits before producing another result. Core never waits for mailbox capacity.
const WAVE_CAPACITY: usize = 16;
// Audition replacement is user-driven rather than part of a wave; this headroom
// keeps ordinary switching from occupying capacity a wave relies on.
const CAPACITY: usize = WAVE_CAPACITY + 2;

type Release = mpsc::Sender<std::result::Result<(), String>>;

// The fixed mailbox keeps payloads inline to avoid allocating on Core.
#[expect(
    clippy::large_enum_variant,
    reason = "bounded inline retirement mailbox"
)]
enum Work {
    Retire(Box<ProcessingResult>),
    RetireRetained(RetainedResult),
    RetirePcm(audio::PcmAudio),
    RetireAudition(RetainedAudition),
    ReleaseAudio {
        outputs: AudioOutputs,
        graph_outputs: BTreeMap<GraphPort, GraphAudioOutput>,
        runner: Option<audio::GraphRunner>,
    },
    #[cfg(test)]
    Hold {
        resume: mpsc::Receiver<()>,
        events: Sender<Event>,
    },
    Barrier(mpsc::Sender<std::result::Result<(), String>>),
}

pub(super) struct Reclaimer {
    sender: Option<mpsc::SyncSender<Work>>,
    worker: Option<JoinHandle<()>>,
    #[cfg(test)]
    pub(super) fail_spawns: bool,
}

impl Work {
    fn execute(self) {
        match self {
            Work::Retire(result) => drop(result),
            Work::RetireRetained(result) => drop(result),
            Work::RetirePcm(pcm) => drop(pcm),
            Work::RetireAudition(audition) => drop(audition),
            Work::ReleaseAudio {
                outputs,
                graph_outputs,
                runner,
            } => drop((outputs, graph_outputs, runner)),
            #[cfg(test)]
            Work::Hold { resume, events } => {
                while !events.is_closed() {
                    match resume.recv_timeout(std::time::Duration::from_millis(10)) {
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        _ => break,
                    }
                }
            }
            Work::Barrier(reply) => {
                let _ = reply.send(Ok(()));
            }
        }
    }
}

impl Reclaimer {
    pub(super) fn new() -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Work>(CAPACITY);
        let worker = thread::Builder::new()
            .name("rivu-processing-reclaim".into())
            .spawn(move || {
                while let Ok(work) = receiver.recv() {
                    work.execute();
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            worker: Some(worker),
            #[cfg(test)]
            fail_spawns: false,
        })
    }

    /// Never waits on the business thread. A full or stopped mailbox degrades
    /// to inline execution: a latency cost, never a Core failure.
    fn send(&self, work: Work) -> bool {
        let work = match &self.sender {
            Some(sender) => match sender.try_send(work) {
                Ok(()) => return true,
                Err(mpsc::TrySendError::Full(work) | mpsc::TrySendError::Disconnected(work)) => {
                    work
                }
            },
            None => work,
        };
        work.execute();
        false
    }

    /// Wave work is bounded by job ownership; overflow means that derivation
    /// broke, which tests must catch even though release builds degrade.
    fn send_wave(&self, work: Work) {
        if !self.send(work) {
            tracing::warn!("processing_reclaim_inline");
            debug_assert!(false, "processing retirement ownership bound exceeded");
        }
    }

    pub(super) fn retire(&self, result: Box<ProcessingResult>) {
        self.send_wave(Work::Retire(result));
    }
    pub(super) fn retire_retained(&self, result: RetainedResult) {
        self.send_wave(Work::RetireRetained(result));
    }
    pub(super) fn retire_pcm(&self, pcm: audio::PcmAudio) {
        self.send_wave(Work::RetirePcm(pcm));
    }
    pub(super) fn retire_audition(&self, audition: RetainedAudition) {
        self.send(Work::RetireAudition(audition));
    }
    pub(super) fn release_audio(
        &self,
        outputs: AudioOutputs,
        graph_outputs: BTreeMap<GraphPort, GraphAudioOutput>,
        runner: Option<audio::GraphRunner>,
    ) {
        self.send_wave(Work::ReleaseAudio {
            outputs,
            graph_outputs,
            runner,
        });
    }

    #[cfg(test)]
    pub(super) fn hold(&self, events: Sender<Event>) -> mpsc::Sender<()> {
        let (release, resume) = mpsc::channel();
        self.send(Work::Hold { resume, events });
        release
    }

    pub(super) fn spawn(
        &self,
        name: &str,
        work: impl FnOnce(Result<()>) + Send + 'static,
    ) -> Result<JoinHandle<()>> {
        let (worker, release) = self.spawn_waiting(name, work)?;
        self.resume_after_release(release);
        Ok(worker)
    }

    pub(super) fn spawn_waiting(
        &self,
        name: &str,
        work: impl FnOnce(Result<()>) + Send + 'static,
    ) -> Result<(JoinHandle<()>, Release)> {
        let (sender, receiver) = mpsc::channel::<std::result::Result<(), String>>();
        #[cfg(test)]
        if self.fail_spawns {
            return Err(std::io::Error::other("injected processing worker spawn failure").into());
        }
        let worker = thread::Builder::new().name(name.into()).spawn(move || {
            let retired = receiver
                .recv()
                .context("Processing reclaimer stopped")
                .and_then(|result| result.map_err(anyhow::Error::msg));
            work(retired);
        })?;
        Ok((worker, sender))
    }

    pub(super) fn resume_after_release(
        &self,
        sender: mpsc::Sender<std::result::Result<(), String>>,
    ) {
        // Only successful admission queues a barrier; failed spawns consume no
        // mailbox capacity and cannot revoke previously retained PCM. An inline
        // fallback releases the worker early: it may then hit the PCM budget,
        // which fails that task rather than Core.
        self.send_wave(Work::Barrier(sender));
    }

    pub(super) fn shutdown(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Reclaimer {
    fn drop(&mut self) {
        self.shutdown();
    }
}
