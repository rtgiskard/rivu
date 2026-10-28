use super::*;
use std::sync::mpsc;

// Four retained outputs, one active result and eight buffered events bound a
// retirement wave. Only a successfully spawned worker submits a barrier, and it
// waits before producing another result. Core never waits for mailbox capacity.
const CAPACITY: usize = 16;

type Release = mpsc::Sender<std::result::Result<(), String>>;

// The fixed 16-slot mailbox keeps payloads inline to avoid allocating on Core.
#[expect(
    clippy::large_enum_variant,
    reason = "bounded inline retirement mailbox"
)]
enum Work {
    Retire(Box<ProcessingResult>),
    RetirePcm(audio::PcmAudio),
    ReleaseAudio {
        outputs: AudioOutputs,
        graph_outputs: BTreeMap<GraphPort, GraphAudioOutput>,
        mix: Option<audio::PcmAudio>,
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

impl Reclaimer {
    pub(super) fn new() -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(CAPACITY);
        let worker = thread::Builder::new()
            .name("rivu-processing-reclaim".into())
            .spawn(move || {
                while let Ok(work) = receiver.recv() {
                    match work {
                        Work::Retire(result) => drop(result),
                        Work::RetirePcm(pcm) => drop(pcm),
                        Work::ReleaseAudio {
                            outputs,
                            graph_outputs,
                            mix,
                            runner,
                        } => drop((outputs, graph_outputs, mix, runner)),
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
            })?;
        Ok(Self {
            sender: Some(sender),
            worker: Some(worker),
            #[cfg(test)]
            fail_spawns: false,
        })
    }

    fn send(&self, work: Work) {
        // Only a single admitted job can produce a retirement wave. A full
        // mailbox would violate that ownership bound, rather than request a
        // blocking fallback on the business thread.
        assert!(
            self.sender.as_ref().unwrap().try_send(work).is_ok(),
            "processing retirement ownership bound exceeded"
        );
    }

    pub(super) fn retire(&self, result: Box<ProcessingResult>) {
        self.send(Work::Retire(result));
    }
    pub(super) fn retire_pcm(&self, pcm: audio::PcmAudio) {
        self.send(Work::RetirePcm(pcm));
    }
    pub(super) fn release_audio(
        &self,
        outputs: AudioOutputs,
        graph_outputs: BTreeMap<GraphPort, GraphAudioOutput>,
        mix: Option<audio::PcmAudio>,
        runner: Option<audio::GraphRunner>,
    ) {
        self.send(Work::ReleaseAudio {
            outputs,
            graph_outputs,
            mix,
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
        // mailbox capacity and cannot revoke previously retained PCM.
        self.send(Work::Barrier(sender));
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
