extern crate core;

use chrono::prelude::*;
use core_affinity::CoreId;
use once_cell::sync::OnceCell;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, UNIX_EPOCH};
use ufmt::uwrite;

use symlink::{remove_symlink_auto, symlink_auto};

pub mod internal;
pub mod macros;

pub static GLOBAL_LOGGER: OnceCell<Logger> = OnceCell::new();

thread_local! {
    pub static TID: std::cell::Cell<u32> =  std::cell::Cell::new(gettid::gettid() as u32) ;
}

thread_local! {
    pub static CACHED_DATE_TIME_STR : std::cell::Cell<PreFormatedDateTime> = std::cell::Cell::new(PreFormatedDateTime::new());
}

pub struct PreFormatedDateTime {
    pub cached: std::cell::RefCell<(
        u64,    /* unix_timestamp_sec */
        String, /* date_time_str_without_subsec */
    )>,
}

impl PreFormatedDateTime {
    pub fn new() -> Self {
        let time = std::time::SystemTime::now();
        let unix_timestamp_sec =
            time.duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64 / 1_000_000_000;
        let date_time: DateTime<Local> = time.into();
        let date_time_str_without_subsec = date_time.format("%H:%M:%S.").to_string();
        Self {
            cached: std::cell::RefCell::new((unix_timestamp_sec, date_time_str_without_subsec)),
        }
    }
    pub fn write_date_time_str(
        &self,
        system_time: std::time::SystemTime,
        rolling_logger: &mut RollingLogger,
    ) {
        let unix_timestamp_ns = system_time.duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
        let now_sec: u64 = unix_timestamp_ns / 1_000_000_000;
        let mut cached = self.cached.borrow_mut();
        if now_sec != cached.0 {
            let local_date_time: DateTime<Local> = system_time.into();
            let _ = rolling_logger.rollate_with_datetime(&local_date_time);
            cached.0 = now_sec;
            cached.1 = local_date_time.format("%H:%M:%S.").to_string();
        }
        rolling_logger.write_buffer(cached.1.as_bytes());
        uwrite!(
            rolling_logger,
            "{}",
            unix_timestamp_ns - (now_sec * 1_000_000_000)
        )
        .unwrap();
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Trace = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
    Off = 99,
}
impl From<&str> for LogLevel {
    fn from(value: &str) -> Self {
        match value {
            "TRACE" | "trace" | "Trace" => LogLevel::Trace,
            "DEBUG" | "debug" | "Debug" => LogLevel::Debug,
            "INFO" | "info" | "Info" => LogLevel::Info,
            "WARN" | "warn" | "Warn" => LogLevel::Warn,
            "ERROR" | "error" | "Error" => LogLevel::Error,
            "OFF" | "off" | "Off" => LogLevel::Off,
            _ => LogLevel::Info,
        }
    }
}

impl LogLevel {
    pub fn to_str(&self) -> &'static str {
        match self {
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
            LogLevel::Trace => "TRACE",
            LogLevel::Off => "OFF",
        }
    }
}

pub struct LoggingFunc {
    func: Box<
        dyn Fn(
                &mut RollingLogger,
                &'static str, /* FILE */
                u32,          /* line */
                u32,          /* tid */
                &str,         /* level */
            ) + Send
            + 'static,
    >,
    file: &'static str,
    line: u32,
    tid: u32,
    level: LogLevel,
}

impl LoggingFunc {
    #[allow(dead_code)]
    pub fn new<T>(func: T, file: &'static str, line: u32, tid: u32, lvl: LogLevel) -> LoggingFunc
    where
        T: Fn(&mut RollingLogger, &'static str, u32, u32, &str) + Send + 'static,
    {
        LoggingFunc {
            func: Box::new(func),
            file,
            line,
            tid,
            level: lvl,
        }
    }
    fn invoke(self, rolling_logger: &mut RollingLogger) {
        (self.func)(
            rolling_logger,
            &self.file,
            self.line,
            self.tid,
            self.level.to_str(),
        );
    }
}

pub struct UString(pub String);
impl ufmt::uWrite for UString {
    type Error = core::convert::Infallible;

    fn write_str(&mut self, s: &str) -> Result<(), core::convert::Infallible> {
        self.0.push_str(s);
        Ok(())
    }
}
impl ufmt::uDisplay for UString {
    fn fmt<W>(&self, f: &mut ufmt::Formatter<'_, W>) -> Result<(), W::Error>
    where
        W: ufmt::uWrite + ?Sized,
    {
        <str as ufmt::uDisplay>::fmt(&self.0, f)
    }
}

/// Determines how often a file should be rolled over
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RollingFrequency {
    EveryDay,
    EveryHour,
    EveryMinute,
}

impl RollingFrequency {
    /// Calculates a datetime that will be different if data should be in
    /// different files.
    pub fn equivalent_datetime(&self, dt: &DateTime<Local>) -> DateTime<Local> {
        match self {
            RollingFrequency::EveryDay => Local
                .with_ymd_and_hms(dt.year(), dt.month(), dt.day(), 0, 0, 0)
                .unwrap(),
            RollingFrequency::EveryHour => Local
                .with_ymd_and_hms(dt.year(), dt.month(), dt.day(), dt.hour(), 0, 0)
                .unwrap(),
            RollingFrequency::EveryMinute => Local
                .with_ymd_and_hms(dt.year(), dt.month(), dt.day(), dt.hour(), dt.minute(), 0)
                .unwrap(),
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RollingCondition {
    last_write_opt: Option<DateTime<Local>>,
    frequency_opt: Option<RollingFrequency>,
    max_size_opt: Option<u64>,
}

impl RollingCondition {
    /// Constructs a new struct that does not yet have any condition set.
    pub fn new() -> RollingCondition {
        RollingCondition {
            last_write_opt: Some(Local::now()),
            frequency_opt: None,
            max_size_opt: None,
        }
    }

    /// Sets a condition to rollover on the given frequency
    pub fn frequency(mut self, x: RollingFrequency) -> RollingCondition {
        self.frequency_opt = Some(x);
        self
    }

    /// Sets a condition to rollover when the date changes
    pub fn daily(mut self) -> RollingCondition {
        self.frequency_opt = Some(RollingFrequency::EveryDay);
        self
    }

    /// Sets a condition to rollover when the date or hour changes
    pub fn hourly(mut self) -> RollingCondition {
        self.frequency_opt = Some(RollingFrequency::EveryHour);
        self
    }

    pub fn minutely(mut self) -> RollingCondition {
        self.frequency_opt = Some(RollingFrequency::EveryMinute);
        self
    }

    /// Sets a condition to rollover when a certain size is reached
    pub fn max_size(mut self, x: u64) -> RollingCondition {
        self.max_size_opt = Some(x);
        self
    }
}

impl RollingCondition {
    fn should_rollover(&mut self, now: &DateTime<Local>, current_filesize: u64) -> bool {
        let mut rollover = false;
        if let Some(frequency) = self.frequency_opt.as_ref() {
            if let Some(last_write) = self.last_write_opt.as_ref() {
                if frequency.equivalent_datetime(now) != frequency.equivalent_datetime(last_write) {
                    rollover = true;
                }
            }
        }
        if let Some(max_size) = self.max_size_opt.as_ref() {
            if current_filesize >= *max_size {
                rollover = true;
            }
        }
        self.last_write_opt = Some(*now);
        rollover
    }
}

pub struct RollingLogger {
    condition: RollingCondition,
    prefix: String,
    folder: String,
    max_files: usize,
    writer_buffer: Option<BufWriter<File>>,
    current_file_size: u64,
}

impl RollingLogger {
    pub fn new(rc: RollingCondition, folder: String, prefix: String, max_files: usize) -> Self {
        if std::fs::metadata(&folder).is_err() {
            std::fs::create_dir_all(&folder).expect("Failed to create log folder");
        }
        let mut rolling_logger = RollingLogger {
            condition: rc,
            prefix,
            folder,
            max_files,
            writer_buffer: None,
            current_file_size: 0,
        };
        rolling_logger
            .open_writer_if_needed(&Local::now())
            .expect("Failed to open log file");
        rolling_logger
    }
}

pub struct Logger {
    rc: RollingCondition,
    folder: String,
    prefix: String,
    max_files: usize,
    cpu: Option<usize>,
    buffer_size: usize,
    filter_level: LogLevel,
    sleep_duration_millis: u64,
    thread_name: String,
    sender: Option<crossbeam_channel::Sender<LoggingFunc>>,
}

impl Logger {
    pub fn new(
        max_queue_size: usize,
        rc: RollingCondition,
        folder: String,
        prefix: String,
        max_files: usize,
        max_level: LogLevel,
        cpu: Option<usize>,
    ) -> Self {
        Logger {
            rc,
            folder,
            prefix,
            max_files,
            cpu,
            buffer_size: max_queue_size,
            filter_level: max_level,
            sleep_duration_millis: 100,
            thread_name: String::from("fastlog"),
            sender: None,
        }
    }

    pub fn init(mut self) -> io::Result<()> {
        let (tx, rx) = match self.buffer_size {
            0 => crossbeam_channel::unbounded(),
            _ => crossbeam_channel::bounded(self.buffer_size),
        };

        self.sender = Some(tx);

        let mut rolling_logger = RollingLogger::new(
            self.rc.clone(),
            self.folder.clone(),
            self.prefix.clone(),
            self.max_files,
        );

        let _a = thread::Builder::new()
            .name(self.thread_name.to_string())
            .spawn(move || {
                if let Some(core) = self.cpu {
                    core_affinity::set_for_current(CoreId { id: core });
                }
                loop {
                    match rx.try_recv() {
                        Ok(cmd) => {
                            Self::process_log_command(cmd, &mut rolling_logger);
                        }
                        Err(e) => match e {
                            crossbeam_channel::TryRecvError::Empty => {
                                {
                                    //TODO: flush
                                    // let mut rolling_log =
                                    //     GLOBAL_ROLLING_LOGGER.get().unwrap().lock().unwrap();
                                    // let _ = rolling_log.flush();
                                }
                                thread::sleep(Duration::from_millis(self.sleep_duration_millis));
                            }
                            crossbeam_channel::TryRecvError::Disconnected => {
                                // let _ = buffered_writer
                                //     .write_all("Logging channel disconnected".as_bytes());
                            }
                        },
                    }
                }
            });

        let _ = GLOBAL_LOGGER.set(self);

        Ok(())
    }

    fn process_log_command(cmd: LoggingFunc, rolling_logger: &mut RollingLogger) {
        cmd.invoke(rolling_logger);
    }

    pub fn log(&self, level: LogLevel, func: LoggingFunc) {
        if level >= self.filter_level {
            match &self.sender {
                Some(tx) => {
                    tx.send(func).unwrap();
                }
                None => (),
            }
        }
    }
}

impl RollingLogger {
    fn flush(&mut self) -> io::Result<()> {
        if let Some(writer) = self.writer_buffer.as_mut() {
            writer.flush()?;
        }
        Ok(())
    }
    pub fn rollover(&mut self) -> io::Result<()> {
        self.flush()?;
        // We must close the current file before rotating files
        self.writer_buffer.take();
        self.current_file_size = 0;
        Ok(())
    }

    fn new_file_name(&self, now: &DateTime<Local>) -> String {
        let mut str = String::with_capacity(self.prefix.len() + 16);
        str.push_str(self.prefix.as_str());
        str.push('.');
        str.push_str(now.format("%Y%m%d.%H%M%S").to_string().as_str());
        str
    }
    /// Opens a writer for the current file.
    fn open_writer_if_needed(&mut self, now: &DateTime<Local>) -> io::Result<()> {
        if self.writer_buffer.is_none() {
            let p = self.new_file_name(now);
            let new_file_path = std::path::Path::new(&self.folder).join(&p);
            if std::fs::metadata(&self.folder).is_err() {
                std::fs::create_dir_all(&self.folder)?;
            }
            let f = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&new_file_path)?;
            self.writer_buffer = Some(BufWriter::new(f));
            // make a soft link to latest file
            {
                let folder = std::path::Path::new(&self.folder);
                if let Ok(path) = folder.canonicalize() {
                    let latest_log_symlink = path.join(&self.prefix);
                    let _ = remove_symlink_auto(folder.join(&self.prefix));
                    let _ = symlink_auto(new_file_path.canonicalize().unwrap(), latest_log_symlink);
                }
            }
            self.current_file_size = std::fs::metadata(&p).map_or(0, |m| m.len());
            self.check_and_remove_log_file()?;
        }
        Ok(())
    }

    pub fn rollate_with_datetime(&mut self, time_point: &DateTime<Local>) -> io::Result<()> {
        if self
            .condition
            .should_rollover(time_point, self.current_file_size)
        {
            if let Err(e) = self.rollover() {
                eprintln!("WARNING: Failed to rotate logfile  {}", e);
            }
        }
        self.open_writer_if_needed(time_point)?;
        Ok(())
    }

    pub fn write_buffer(&mut self, buf: &[u8]) {
        // if self.condition.should_rollover(now, self.current_file_size) {
        //     if let Err(e) = self.rollover() {
        //         eprintln!("WARNING: Failed to rotate logfile  {}", e);
        //     }
        // }
        // self.open_writer_if_needed(now)?;

        let writer = self.writer_buffer.as_mut().unwrap();
        let buf_len = buf.len();
        let _ = writer.write_all(buf).map(|_| {
            self.current_file_size += u64::try_from(buf_len).unwrap_or(u64::MAX);
            buf_len
        });
    }

    fn check_and_remove_log_file(&mut self) -> io::Result<()> {
        let files = std::fs::read_dir(&self.folder)?;

        let mut log_files = vec![];
        for f in files.flatten() {
            let fname = f.file_name().to_string_lossy().to_string();
            if fname.starts_with(&self.prefix) && fname != self.prefix {
                log_files.push(fname);
            }
        }

        log_files.sort_by(|a, b| b.cmp(a));

        if log_files.len() > self.max_files {
            for f in log_files.drain(self.max_files..) {
                let p = Path::new(&self.folder).join(f);
                if let Err(e) = fs::remove_file(&p) {
                    eprintln!(
                        "WARNING: Failed to remove old logfile {}: {}",
                        p.to_string_lossy(),
                        e
                    );
                }
            }
        }
        Ok(())
    }
}

impl ufmt::uWrite for RollingLogger {
    type Error = core::convert::Infallible;

    fn write_str(&mut self, s: &str) -> Result<(), core::convert::Infallible> {
        let _ = self.write_buffer(s.as_bytes());
        Ok(())
    }
}

impl Drop for Logger {
    fn drop(&mut self) {}
}

pub fn logger() -> &'static Logger {
    GLOBAL_LOGGER.get().unwrap()
}

#[cfg(test)]
mod tests {
    use crate::{debug, error, info, warn};

    #[test]
    pub fn test_log() {}
}
