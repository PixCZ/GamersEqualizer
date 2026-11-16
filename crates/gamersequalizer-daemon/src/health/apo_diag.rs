//! What the log and the window learn about our effects: `[ApoCheck]`, `[ApoNote]` and `run_status.json`.

use super::*;

/// The service's verdict on our effects really running, read by the window (`RunStatus`).
const RUN_FILE: &str = "run_status.json";
/// APO notes handed to the window.
const NOTES_FOR_WINDOW: usize = 40;
/// How often the Application log is queried for new APO notes.
const NOTES_INTERVAL: Duration = Duration::from_secs(30);

/// What the service found about our effects really running, for the window
/// (`run_status.json` next to `health.json`): header, Mic tab and notification say the same.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunStatus {
  /// Device the mic effect is listed on but did not run on while an app recorded from it.
  pub mic_not_running: Option<String>,
  /// Same for the output EQ while sound played.
  pub output_not_running: Option<String>,
  /// Newest notes from inside audiodg (`apo_notes`), newest first, "time text".
  pub apo_notes: Vec<String>,
}

impl RunStatus {
  /// Missing or unreadable -> nothing known (the window then claims nothing either way).
  pub fn load(dir: &Path) -> Self {
    std::fs::read_to_string(dir.join(RUN_FILE))
      .ok()
      .and_then(|text| serde_json::from_str(&text).ok())
      .unwrap_or_default()
  }

  pub fn path(dir: &Path) -> PathBuf {
    dir.join(RUN_FILE)
  }

  fn save(&self, dir: &Path) {
    let path = dir.join(RUN_FILE);
    let result =
      serde_json::to_string_pretty(self).map_err(std::io::Error::other).and_then(|json| write_atomic(&path, &json));
    if let Err(e) = result {
      warn!("[Health] could not save {}: {e}", path.display());
    }
  }
}

impl HealthMonitor {
  /// Logs what decides whether Windows runs our effects (`[ApoCheck]`: system, registration,
  /// every device with our effects, the devices in use) and new notes (`[ApoNote]`: from inside
  /// audiodg, and what Windows recorded about our files), each only when it changes, so a
  /// tester's log alone tells what went wrong.
  pub(super) fn log_apo_state(&mut self, mic: Option<&str>, output: Option<&str>) {
    let mut lines = vec![apo_install::describe_system(), apo_install::describe_installed()];
    for (flow, id) in [(Flow::Capture, mic), (Flow::Render, output)] {
      if let Some(guid) = id.and_then(apo_install::endpoint_guid_of) {
        lines.push(apo_install::describe_endpoint(flow, &guid));
      }
    }
    for line in lines.iter().filter(|l| !self.described.contains(*l)) {
      info!("[ApoCheck] {line}");
    }
    self.described = lines;

    let finished = match self.notes_reading.as_ref().map(|rx| rx.try_recv()) {
      None => None,
      Some(Ok(read)) => Some(read),
      Some(Err(TryRecvError::Empty)) => return,
      Some(Err(TryRecvError::Disconnected)) => Some(None),
    };
    if let Some(read) = finished {
      self.notes_reading = None;
      // Unreadable this time: keep the last ones (else they would be logged again next time).
      if let Some(notes) = read {
        // Each note once, also across restarts: every start used to log the newest 100 again.
        // One without a time ("... not readable") once per run.
        let until = self.memory.notes_logged_until.clone();
        // Oldest first, so the log reads in order.
        for note in notes.iter().rev().filter(|n| !self.notes.contains(*n)) {
          if note.time.is_empty() || note.time > until {
            info!("[ApoNote] {} {}", note.time, note.text);
          }
        }
        if let Some(newest) = notes.iter().map(|n| &n.time).max().filter(|t| **t > until) {
          self.memory.notes_logged_until = newest.clone();
          self.save();
        }
        self.notes = notes;
      }
    }

    // A query scans whole logs: not every check.
    if self.notes_read_at.is_some_and(|t| t.elapsed() < NOTES_INTERVAL) {
      return;
    }
    self.notes_read_at = Some(Instant::now());
    let (tx, rx) = mpsc::channel();
    let started = std::thread::Builder::new().name("apo-notes".into()).spawn(move || {
      let read = apo_notes::newest().zip(apo_notes::windows_events()).map(|(mut notes, windows)| {
        notes.extend(windows);
        // Newest first, like the notes themselves (ISO times sort as text).
        notes.sort_by(|a, b| b.time.cmp(&a.time));
        notes
      });
      let _ = tx.send(read);
    });
    match started {
      Ok(_) => self.notes_reading = Some(rx),
      Err(e) => warn!("[Health] could not start reading the event logs: {e}"),
    }
  }

  /// Writes `run_status.json` when the verdict or the notes changed.
  pub(super) fn publish_run_status(&mut self) {
    let not_running = |wanted: Flow| {
      self.mic_shown.iter().find_map(|issue| match issue {
        Issue::NotRunning { flow, device_name, .. } if *flow == wanted => Some(device_name.clone()),
        _ => None,
      })
    };
    let status = RunStatus {
      mic_not_running: not_running(Flow::Capture),
      output_not_running: not_running(Flow::Render),
      apo_notes: self.notes.iter().take(NOTES_FOR_WINDOW).map(|n| format!("{} {}", n.time, n.text)).collect(),
    };
    if self.run_status.as_ref() != Some(&status) {
      status.save(self.dir());
      self.run_status = Some(status);
    }
  }
}
