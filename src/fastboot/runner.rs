use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::manifest::{Operation, RebootTarget};

/// Resolve a deliberately selected executable, never an executable from PATH.
pub fn resolve_fastboot(root: &Path, explicit: Option<&Path>) -> Result<PathBuf, String> {
    let path = match explicit {
        Some(path) => path.to_path_buf(),
        None => {
            let (platform, executable) = if cfg!(target_os = "windows") {
                ("windows", "fastboot.exe")
            } else if cfg!(target_os = "macos") {
                ("macos", "fastboot")
            } else if cfg!(target_os = "linux") {
                ("linux", "fastboot")
            } else {
                return Err("No bundled fastboot location for this operating system; use --fastboot with an executable file path.".into());
            };
            root.join("bin").join(platform).join(executable)
        }
    };

    if !path.is_file() {
        return Err(format!(
            "Fastboot executable not found at {}. Install platform-tools in the package's bin directory or use --fastboot with its full file path.",
            path.display()
        ));
    }
    let path = fs::canonicalize(&path).map_err(|error| {
        format!(
            "Cannot resolve fastboot executable {}: {error}",
            path.display()
        )
    })?;
    // On Windows, Command may hand a batch file to cmd.exe. Only native .exe
    // files are accepted so package scripts are never interpreted by a shell.
    if cfg!(target_os = "windows")
        && !path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    {
        return Err("The fastboot executable must be a native .exe file on Windows.".into());
    }
    Ok(path)
}

pub struct Fastboot {
    executable: PathBuf,
    requested_serial: Option<String>,
    connected: bool,
    timeout: Duration,
    flash_timeout: Duration,
    transport: Box<dyn Transport>,
}

impl Fastboot {
    pub fn new(executable: PathBuf, serial: Option<String>, timeout: Duration) -> Self {
        Self {
            executable,
            requested_serial: serial,
            connected: false,
            timeout,
            flash_timeout: Duration::from_secs(1800),
            transport: Box::new(ProcessTransport),
        }
    }

    pub fn with_flash_timeout(mut self, timeout: Duration) -> Self {
        self.flash_timeout = timeout;
        self
    }

    /// Perform read-only checks. No reboot, erase, slot change or flash occurs.
    pub fn connect(
        &mut self,
        products: &[String],
        needs_country: bool,
    ) -> Result<Option<String>, String> {
        self.connected = false;
        if self.timeout.is_zero() {
            return Err("Fastboot timeout must be greater than zero.".into());
        }
        if products.is_empty() {
            return Err("The manifest must specify at least one supported product.".into());
        }
        let product = self.getvar("product", self.timeout)?;
        if !products.iter().any(|supported| supported == &product) {
            return Err(format!(
                "Device product {product:?} does not match the manifest ({}). No changes were made.",
                products.join(", ")
            ));
        }
        println!("Verified product: {product}");

        match self.getvar("is-userspace", self.timeout)?.as_str() {
            "no" => {}
            "yes" => return Err("The device is in fastbootd. Start in bootloader fastboot mode before flashing. No changes were made.".into()),
            value => return Err(format!("Cannot verify bootloader mode: unexpected is-userspace value {value:?}. No changes were made.")),
        }

        let country = if needs_country {
            let result = self.run_selected(
                &[OsString::from("oem"), OsString::from("hwid")],
                self.timeout,
                false,
            )?;
            result.require_success("Read device hardware country with oem hwid")?;
            let country = parse_variable(&result.combined(), "hwcountry")?
                .ok_or_else(|| "The device did not report hwcountry in oem hwid; cannot select regional images safely.".to_string())?;
            if country.is_empty()
                || !country.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
                })
            {
                return Err(format!(
                    "Invalid hardware country reported by the device: {country:?}."
                ));
            }
            println!("Verified hardware country: {country}");
            Some(country)
        } else {
            None
        };
        self.connected = true;
        Ok(country)
    }

    /// Execute only after the caller has displayed and confirmed the full plan.
    pub fn execute(&mut self, operations: &[Operation]) -> Result<(), String> {
        if !self.connected {
            return Err("Connect and verify a device before executing a flash plan.".into());
        }
        if self.flash_timeout.is_zero() {
            return Err("Flash command timeout must be greater than zero.".into());
        }
        // Validate the entire plan before its first mutation, including paths.
        for operation in operations {
            validate_operation(operation)?;
        }
        for (index, operation) in operations.iter().enumerate() {
            let (args, description) = match operation {
                Operation::SetActive(slot) => (
                    vec![OsString::from(format!("--set-active={slot}"))],
                    format!("Set active slot {slot}"),
                ),
                Operation::Flash { partition, image } => (
                    vec![
                        OsString::from("flash"),
                        OsString::from(partition),
                        image.as_os_str().to_os_string(),
                    ],
                    format!("Flash {partition} from {}", image.display()),
                ),
                Operation::Erase(partition) => (
                    vec![OsString::from("erase"), OsString::from(partition)],
                    format!("Erase {partition}"),
                ),
                Operation::Reboot(target) => match target {
                    RebootTarget::Fastboot => (
                        vec![OsString::from("reboot"), OsString::from("fastboot")],
                        "Reboot to fastbootd".into(),
                    ),
                    RebootTarget::Bootloader => (
                        vec![OsString::from("reboot"), OsString::from("bootloader")],
                        "Reboot to bootloader".into(),
                    ),
                    RebootTarget::System => {
                        (vec![OsString::from("reboot")], "Reboot to system".into())
                    }
                },
            };
            println!("[{}/{}] {description}", index + 1, operations.len());
            // Reboot plus every subsequent readiness probe share one deadline.
            let started = Instant::now();
            let command_timeout = if matches!(operation, Operation::Flash { .. }) {
                self.flash_timeout
            } else {
                self.timeout
            };
            let result = self.run_selected(&args, command_timeout, true)?;
            result.require_success(&description)?;
            if let Operation::Reboot(target) = operation {
                match target {
                    RebootTarget::Fastboot | RebootTarget::Bootloader => {
                        let remaining = self.timeout.saturating_sub(started.elapsed());
                        self.wait_for_mode(target, remaining)?;
                    }
                    RebootTarget::System => {}
                }
            }
        }
        Ok(())
    }

    fn run_unselected(
        &mut self,
        args: &[OsString],
        timeout: Duration,
        stream: bool,
    ) -> Result<CommandResult, String> {
        self.transport.run(&self.executable, args, timeout, stream)
    }

    fn run_selected(
        &mut self,
        args: &[OsString],
        timeout: Duration,
        stream: bool,
    ) -> Result<CommandResult, String> {
        let mut selected = Vec::new();
        if let Some(serial) = &self.requested_serial {
            selected.extend([OsString::from("-s"), OsString::from(serial)]);
        }
        selected.extend_from_slice(args);
        self.run_unselected(&selected, timeout, stream)
    }

    fn getvar(&mut self, variable: &str, timeout: Duration) -> Result<String, String> {
        let result = self.run_selected(
            &[OsString::from("getvar"), OsString::from(variable)],
            timeout,
            false,
        )?;
        result.require_success(&format!("Read fastboot variable {variable}"))?;
        parse_variable(&result.combined(), variable)?
            .ok_or_else(|| format!("Fastboot did not report required variable {variable}."))
    }

    fn wait_for_mode(&mut self, target: &RebootTarget, timeout: Duration) -> Result<(), String> {
        let expected = match target {
            RebootTarget::Fastboot => "yes",
            RebootTarget::Bootloader => "no",
            RebootTarget::System => return Ok(()),
        };
        let mode = if expected == "yes" {
            "fastbootd"
        } else {
            "bootloader"
        };
        let started = Instant::now();
        let mut last_error = "The device has not reconnected.".to_string();
        loop {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(format!(
                    "Timed out waiting for the device to enter {mode}. {last_error}"
                ));
            }
            // A missing device can make fastboot wait forever. Each probe is
            // killed after at most three seconds. Any caller-supplied selector
            // is forwarded as usual; the runner never queries device serials.
            match self.getvar("is-userspace", remaining.min(Duration::from_secs(3))) {
                Ok(value) if value == expected => {
                    println!("Verified {mode} mode.");
                    return Ok(());
                }
                Ok(value) => {
                    last_error = format!("is-userspace is {value:?}; expected {expected:?}.")
                }
                Err(error) => last_error = error,
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            if !remaining.is_zero() {
                thread::sleep(remaining.min(Duration::from_millis(250)));
            }
        }
    }
}

fn validate_operation(operation: &Operation) -> Result<(), String> {
    let partition = match operation {
        Operation::SetActive(slot) if matches!(slot.as_str(), "a" | "b") => return Ok(()),
        Operation::SetActive(slot) => {
            return Err(format!("Invalid active slot {slot:?}; expected a or b."))
        }
        Operation::Flash { partition, image } => {
            if !image.is_absolute() || !image.is_file() {
                return Err(format!(
                    "Flash image must be an existing absolute file path: {}",
                    image.display()
                ));
            }
            partition
        }
        Operation::Erase(partition) => partition,
        Operation::Reboot(_) => return Ok(()),
    };
    if partition.is_empty()
        || partition.starts_with('-')
        || !partition.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        })
    {
        return Err(format!("Invalid fastboot partition name {partition:?}."));
    }
    Ok(())
}

fn parse_variable(output: &str, variable: &str) -> Result<Option<String>, String> {
    let mut found: Option<String> = None;
    for line in output.lines() {
        let mut line = line.trim();
        if let Some(value) = line.strip_prefix("(bootloader)") {
            line = value.trim_start();
        }
        if let Some(value) = line.strip_prefix("INFO") {
            line = value.trim_start();
        }
        let Some((name, value)) = line.split_once(':').or_else(|| line.split_once('=')) else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case(variable) {
            continue;
        }
        let value = value.trim().trim_matches(['\'', '"']).to_string();
        if value.is_empty() {
            continue;
        }
        if let Some(previous) = &found {
            if previous != &value {
                return Err(format!(
                    "Fastboot reported conflicting values for {variable}."
                ));
            }
        } else {
            found = Some(value);
        }
    }
    Ok(found)
}

struct CommandResult {
    success: bool,
    stdout: String,
    stderr: String,
}

impl CommandResult {
    fn combined(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }

    fn require_success(&self, action: &str) -> Result<(), String> {
        if self.success {
            return Ok(());
        }
        let output = self.combined();
        let detail = output.trim();
        if detail.is_empty() {
            Err(format!("{action} failed. Flashing stopped."))
        } else {
            Err(format!("{action} failed. Flashing stopped.\n{detail}"))
        }
    }
}

trait Transport {
    fn run(
        &mut self,
        executable: &Path,
        args: &[OsString],
        timeout: Duration,
        stream: bool,
    ) -> Result<CommandResult, String>;
}

struct ProcessTransport;

impl Transport for ProcessTransport {
    fn run(
        &mut self,
        executable: &Path,
        args: &[OsString],
        timeout: Duration,
        stream: bool,
    ) -> Result<CommandResult, String> {
        if timeout.is_zero() {
            return Err("Fastboot command deadline expired.".into());
        }
        let started = Instant::now();
        let mut child = Command::new(executable)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                format!("Cannot start fastboot at {}: {error}", executable.display())
            })?;
        let stdout = child
            .stdout
            .take()
            .ok_or("Cannot capture fastboot stdout.")?;
        let stderr = child
            .stderr
            .take()
            .ok_or("Cannot capture fastboot stderr.")?;
        let stdout_reader = thread::spawn(move || capture_output(stdout, stream, false));
        let stderr_reader = thread::spawn(move || capture_output(stderr, stream, true));
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if started.elapsed() < timeout => {
                    thread::sleep(
                        (timeout - started.elapsed().min(timeout)).min(Duration::from_millis(20)),
                    );
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(format!(
                        "Fastboot command timed out after {:.1} seconds. Flashing stopped.",
                        timeout.as_secs_f64()
                    ));
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(format!(
                        "Cannot wait for fastboot: {error}. Flashing stopped."
                    ));
                }
            }
        };
        let stdout = stdout_reader
            .join()
            .map_err(|_| "Fastboot stdout reader failed.")??;
        let stderr = stderr_reader
            .join()
            .map_err(|_| "Fastboot stderr reader failed.")??;
        Ok(CommandResult {
            success: status?.success(),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }
}

fn capture_output(mut reader: impl Read, stream: bool, is_stderr: bool) -> Result<Vec<u8>, String> {
    // Keep diagnostic memory bounded while still forwarding every output byte.
    const CAPTURE_LIMIT: usize = 1024 * 1024;
    let mut captured = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) => return Ok(captured),
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("Cannot read fastboot output: {error}")),
        };
        if stream {
            if is_stderr {
                let mut output = io::stderr().lock();
                let _ = output.write_all(&buffer[..count]);
                let _ = output.flush();
            } else {
                let mut output = io::stdout().lock();
                let _ = output.write_all(&buffer[..count]);
                let _ = output.flush();
            }
        }
        if captured.len() + count > CAPTURE_LIMIT {
            let discard = captured.len() + count - CAPTURE_LIMIT;
            captured.drain(..discard);
        }
        captured.extend_from_slice(&buffer[..count]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    struct MockTransport {
        responses: VecDeque<Result<CommandResult, String>>,
        calls: Rc<RefCell<Vec<Vec<String>>>>,
    }

    impl Transport for MockTransport {
        fn run(
            &mut self,
            _executable: &Path,
            args: &[OsString],
            timeout: Duration,
            _stream: bool,
        ) -> Result<CommandResult, String> {
            assert!(!timeout.is_zero());
            self.calls.borrow_mut().push(
                args.iter()
                    .map(|argument| argument.to_string_lossy().into_owned())
                    .collect(),
            );
            self.responses
                .pop_front()
                .expect("unexpected fastboot command")
        }
    }

    fn reply(stdout: &str, stderr: &str) -> Result<CommandResult, String> {
        Ok(CommandResult {
            success: true,
            stdout: stdout.into(),
            stderr: stderr.into(),
        })
    }

    fn mock(
        responses: Vec<Result<CommandResult, String>>,
        serial: Option<&str>,
    ) -> (Fastboot, Rc<RefCell<Vec<Vec<String>>>>) {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut fastboot = Fastboot::new(
            PathBuf::from("fastboot"),
            serial.map(str::to_string),
            Duration::from_millis(25),
        );
        fastboot.transport = Box::new(MockTransport {
            responses: responses.into(),
            calls: calls.clone(),
        });
        (fastboot, calls)
    }

    #[test]
    fn connect_never_enumerates_devices_or_queries_serial_numbers() {
        let (mut fastboot, calls) = mock(
            vec![reply("", "product: onyx"), reply("", "is-userspace: no")],
            None,
        );
        fastboot.connect(&["onyx".into()], false).unwrap();
        assert_eq!(
            *calls.borrow(),
            vec![vec!["getvar", "product"], vec!["getvar", "is-userspace"]]
        );
    }

    #[test]
    fn forwards_an_optional_selector_without_checking_it() {
        let (mut fastboot, calls) = mock(
            vec![
                reply("", "product: onyx\nFinished. Total time: 0.000s"),
                reply("", "is-userspace: no"),
                reply("", "(bootloader) hwcountry: INDIA\nOKAY"),
                reply("", "Setting current slot to 'a' OKAY"),
            ],
            Some("caller selector"),
        );
        assert_eq!(
            fastboot.connect(&["onyx".into()], true).unwrap(),
            Some("INDIA".into())
        );
        fastboot
            .execute(&[Operation::SetActive("a".into())])
            .unwrap();
        let calls = calls.borrow();
        assert!(calls
            .iter()
            .all(|call| call[..2] == ["-s", "caller selector"]));
        assert_eq!(calls[0], ["-s", "caller selector", "getvar", "product"]);
        assert_eq!(calls[3], ["-s", "caller selector", "--set-active=a"]);
    }

    #[test]
    fn product_mismatch_prevents_execution() {
        let (mut fastboot, calls) = mock(vec![reply("", "product: another-device")], None);
        assert!(fastboot.connect(&["onyx".into()], false).is_err());
        assert!(fastboot
            .execute(&[Operation::Erase("userdata".into())])
            .is_err());
        assert_eq!(calls.borrow().len(), 1);
    }

    #[test]
    fn fails_closed_when_bootloader_mode_cannot_be_verified() {
        let (mut fastboot, calls) = mock(
            vec![reply("", "product: onyx"), reply("", "is-userspace: yes")],
            None,
        );
        assert!(fastboot
            .connect(&["onyx".into()], false)
            .unwrap_err()
            .contains("fastbootd"));
        assert!(fastboot
            .execute(&[Operation::Erase("userdata".into())])
            .is_err());
        assert_eq!(calls.borrow().len(), 2);
    }

    #[test]
    fn regional_images_require_unambiguous_hardware_country() {
        assert_eq!(
            parse_variable("(bootloader) hwcountry: INDIA\n", "hwcountry").unwrap(),
            Some("INDIA".into())
        );
        assert!(parse_variable("hwcountry: INDIA\nhwcountry: GLOBAL", "hwcountry").is_err());
        let (mut fastboot, calls) = mock(
            vec![
                reply("", "product: onyx"),
                reply("", "is-userspace: no"),
                reply("", "(bootloader) hwid: 123"),
            ],
            None,
        );
        assert!(fastboot
            .connect(&["onyx".into()], true)
            .unwrap_err()
            .contains("did not report hwcountry"));
        assert!(fastboot
            .execute(&[Operation::SetActive("a".into())])
            .is_err());
        assert_eq!(calls.borrow().len(), 3);
    }

    #[test]
    fn verifies_fastbootd_after_reboot_without_a_serial_query() {
        let (mut fastboot, calls) = mock(
            vec![
                reply("", "product: onyx"),
                reply("", "is-userspace: no"),
                reply("", "Rebooting into fastboot OKAY"),
                reply("", "is-userspace: yes"),
            ],
            None,
        );
        fastboot.connect(&["onyx".into()], false).unwrap();
        fastboot
            .execute(&[Operation::Reboot(RebootTarget::Fastboot)])
            .unwrap();
        let calls = calls.borrow();
        assert_eq!(calls[2], ["reboot", "fastboot"]);
        assert_eq!(calls[3], ["getvar", "is-userspace"]);
    }

    #[test]
    fn transition_timeout_prevents_the_next_mutation() {
        let (mut fastboot, calls) = mock(
            vec![
                reply("", "product: onyx"),
                reply("", "is-userspace: no"),
                reply("", "Rebooting into fastboot OKAY"),
                reply("", "is-userspace: no"),
            ],
            None,
        );
        fastboot.connect(&["onyx".into()], false).unwrap();
        let started = Instant::now();
        let operations = [
            Operation::Reboot(RebootTarget::Fastboot),
            Operation::Erase("userdata".into()),
        ];
        assert!(fastboot
            .execute(&operations)
            .unwrap_err()
            .contains("Timed out"));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(calls.borrow().len(), 4);
    }

    #[test]
    fn stops_at_the_first_failed_mutation() {
        let (mut fastboot, calls) = mock(
            vec![
                reply("", "product: onyx"),
                reply("", "is-userspace: no"),
                Ok(CommandResult {
                    success: false,
                    stdout: String::new(),
                    stderr: "FAILED (remote: locked)".into(),
                }),
            ],
            None,
        );
        fastboot.connect(&["onyx".into()], false).unwrap();
        let operations = [
            Operation::SetActive("a".into()),
            Operation::Erase("userdata".into()),
        ];
        assert!(fastboot
            .execute(&operations)
            .unwrap_err()
            .contains("locked"));
        assert_eq!(calls.borrow().len(), 3);
    }

    #[test]
    fn validates_the_entire_plan_before_mutating() {
        let (mut fastboot, calls) = mock(
            vec![reply("", "product: onyx"), reply("", "is-userspace: no")],
            None,
        );
        fastboot.connect(&["onyx".into()], false).unwrap();
        let operations = [
            Operation::SetActive("a".into()),
            Operation::Erase("--wipe".into()),
        ];
        assert!(fastboot.execute(&operations).is_err());
        assert_eq!(calls.borrow().len(), 2);
    }
}
