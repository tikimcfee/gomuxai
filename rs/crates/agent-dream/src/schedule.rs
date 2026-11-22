//! When dreams run: a recurrence ("daily 04:00", "every 6h") and the systemd
//! user units that carry it out. Turning a schedule on is a person's call;
//! this module only parses and renders.

use anyhow::{Result, bail};

/// How often a dream runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recurrence {
    /// Once a day at this local time.
    Daily { hour: u8, minute: u8 },
    /// Every so many hours, on the hour (a divisor of 24 keeps it even).
    EveryHours(u8),
}

impl Recurrence {
    /// Parses `daily HH:MM` or `every Nh` (N from 1 to 24).
    pub fn parse(words: &[&str]) -> Result<Recurrence> {
        match words {
            ["daily", at] => {
                let Some((h, m)) = at.split_once(':') else {
                    bail!("daily takes a time like 04:00, not {at:?}");
                };
                let (Ok(hour), Ok(minute)) = (h.parse::<u8>(), m.parse::<u8>()) else {
                    bail!("daily takes a time like 04:00, not {at:?}");
                };
                if hour > 23 || minute > 59 || m.len() != 2 {
                    bail!("daily takes a time like 04:00, not {at:?}");
                }
                Ok(Recurrence::Daily { hour, minute })
            }
            ["every", n] => {
                let hours = n
                    .strip_suffix('h')
                    .and_then(|h| h.parse::<u8>().ok())
                    .filter(|h| (1..=24).contains(h));
                match hours {
                    Some(h) => Ok(Recurrence::EveryHours(h)),
                    None => bail!("every takes hours from 1h to 24h, not {n:?}"),
                }
            }
            _ => bail!("give a schedule: daily HH:MM, or every Nh"),
        }
    }

    /// The systemd `OnCalendar=` value.
    pub fn on_calendar(&self) -> String {
        match self {
            Recurrence::Daily { hour, minute } => format!("*-*-* {hour:02}:{minute:02}:00"),
            Recurrence::EveryHours(24) => "*-*-* 00:00:00".to_string(),
            Recurrence::EveryHours(h) => format!("*-*-* 00/{h}:00:00"),
        }
    }

    /// In words, as `agent dream schedule` prints it.
    pub fn describe(&self) -> String {
        match self {
            Recurrence::Daily { hour, minute } => format!("daily at {hour:02}:{minute:02}"),
            Recurrence::EveryHours(1) => "every hour".to_string(),
            Recurrence::EveryHours(h) => format!("every {h} hours"),
        }
    }
}

/// The unit name both files share.
pub const UNIT: &str = "gomuxai-dream";

/// The service unit: one `agent dream run`, with the PATH it was scheduled
/// with (so it finds claude and git) and the agent binary's directory as
/// its working directory.
pub fn service_unit(agent_binary: &str, path_env: &str, workdir: &str) -> String {
    format!(
        "[Unit]\n\
         Description=gomuxai dream (agent dream run)\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         WorkingDirectory={workdir}\n\
         Environment=PATH={path_env}\n\
         ExecStart={agent_binary} dream run\n"
    )
}

/// The timer unit. Persistent: a dream missed while the machine was off runs
/// once when it is back, however many were missed.
pub fn timer_unit(r: &Recurrence) -> String {
    format!(
        "[Unit]\n\
         Description=gomuxai dream, {}\n\
         \n\
         [Timer]\n\
         OnCalendar={}\n\
         Persistent=true\n\
         RandomizedDelaySec=120\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n",
        r.describe(),
        r.on_calendar()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_daily_and_every() {
        assert_eq!(
            Recurrence::parse(&["daily", "04:30"]).unwrap(),
            Recurrence::Daily {
                hour: 4,
                minute: 30
            }
        );
        assert_eq!(
            Recurrence::parse(&["every", "6h"]).unwrap(),
            Recurrence::EveryHours(6)
        );
        for bad in [
            &["daily", "4"][..],
            &["daily", "24:00"],
            &["daily", "04:7"],
            &["every", "0h"],
            &["every", "25h"],
            &["every", "6"],
            &["weekly"],
        ] {
            assert!(Recurrence::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn renders_calendar() {
        assert_eq!(
            Recurrence::Daily { hour: 4, minute: 5 }.on_calendar(),
            "*-*-* 04:05:00"
        );
        assert_eq!(Recurrence::EveryHours(6).on_calendar(), "*-*-* 00/6:00:00");
        assert_eq!(Recurrence::EveryHours(24).on_calendar(), "*-*-* 00:00:00");
        let t = timer_unit(&Recurrence::EveryHours(6));
        assert!(t.contains("Persistent=true") && t.contains("every 6 hours"));
        let s = service_unit("/opt/agent", "/usr/bin", "/opt");
        assert!(s.contains("ExecStart=/opt/agent dream run"));
    }
}
