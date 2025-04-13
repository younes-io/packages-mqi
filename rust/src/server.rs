use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::io::{self, BufRead, BufReader, Read};
use std::thread;
use log::{
    debug, error, info, warn
};

#[cfg(feature = "password-gen")]
use uuid::Uuid;

#[cfg(all(unix, feature="unix-socket"))]
use nix::unistd::mkdtemp;
#[cfg(all(unix, feature="unix-socket"))]
use std::fs;
#[cfg(all(unix, feature="unix-socket"))]
use std::os::unix::fs::PermissionsExt;

use crate::error::PrologError;
use crate::session::{PrologSession, ConnectionAddr};

// Placeholder for PrologServer configuration
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub launch_mqi: bool,
    pub port: Option<u16>,
    pub password: Option<String>,
    // If Some(path) and path is empty, generate UDS path
    pub unix_domain_socket: Option<PathBuf>,
    pub query_timeout_seconds: Option<f64>,
    pub pending_connection_count: Option<u32>,
    pub output_file_name: Option<PathBuf>,
    pub mqi_traces: Option<String>,
    pub prolog_path: Option<PathBuf>,
    pub prolog_path_args: Option<Vec<String>>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            launch_mqi: true,
            port: None,
            password: None, // Will be generated if None and launch_mqi is true and feature enabled
            unix_domain_socket: None,
            query_timeout_seconds: None,
            pending_connection_count: None, // Use Prolog's default (5)
            output_file_name: None,
            mqi_traces: None,
            prolog_path: None, // Assumes 'swipl' is in PATH
            prolog_path_args: None,
        }
    }
}

/// Represents and manages a connection to a SWI-Prolog MQI server process.
#[derive(Debug)]
pub struct PrologServer {
    config: ServerConfig,
    process: Option<Child>,
    // Need Arc<Mutex> for thread safety if accessed by session
    connection_failed: Arc<Mutex<bool>>,
    // Details needed by session to connect
    effective_port: Option<u16>,
    effective_uds_path: Option<PathBuf>,
    effective_password: Option<String>,
    // For cleaning up generated UDS
    generated_uds_dir: Option<PathBuf>,
}

impl PrologServer {
    /// Creates a new PrologServer instance with the given configuration.
    /// This does not start the server process yet; call `start()` for that.
    pub fn new(config: ServerConfig) -> Result<Self, PrologError> {
        // Initial validation
        if config.unix_domain_socket.is_some() {
            #[cfg(not(all(unix, feature="unix-socket")))]
            return Err(PrologError::FeatureNotEnabled(
                "Unix Domain Sockets require the 'unix-socket' feature on Unix-like systems".to_string(),
            ));
            #[cfg(all(unix, feature="unix-socket"))]
            if config.port.is_some() {
                return Err(PrologError::InvalidState(
                    "Cannot specify both port and unix_domain_socket".to_string(),
                ));
            }
        }

        if !config.launch_mqi && config.output_file_name.is_some() {
            return Err(PrologError::InvalidState(
                "output_file_name only works when launch_mqi is true".to_string(),
            ));
        }

        // Standalone mode validation
        if !config.launch_mqi && (config.port.is_none() && config.unix_domain_socket.is_none()) {
             return Err(PrologError::InvalidState(
                "Must specify port or unix_domain_socket when launch_mqi is false".to_string(),
            ));
        }
         if !config.launch_mqi && config.password.is_none() {
             return Err(PrologError::InvalidState(
                "Must specify password when launch_mqi is false".to_string(),
            ));
        }

        Ok(Self {
            effective_port: config.port,
            effective_uds_path: config.unix_domain_socket.clone(), // Clone path if provided
            effective_password: config.password.clone(),
            config,
            process: None,
            connection_failed: Arc::new(Mutex::new(false)),
            generated_uds_dir: None,
        })
    }

    /// Starts the SWI-Prolog MQI server process if `launch_mqi` is true.
    /// If `launch_mqi` is false, this method does nothing but basic validation.
    pub fn start(&mut self) -> Result<(), PrologError> {
        if !self.config.launch_mqi {
            info!("Running in standalone mode, not launching swipl.");
            // Already validated connection details in new()
            return Ok(());
        }

        // Ensure process isn't already running
        if self.process.is_some() {
            return Err(PrologError::InvalidState("Server process already started".to_string()));
        }

        info!("Starting SWI-Prolog MQI process...");

        // Validate and get the Prolog executable path
        let prolog_executable = self.config.prolog_path.as_ref().ok_or_else(|| {
            PrologError::ConfigError("Path to SWI-Prolog executable (prolog_path) not configured".to_string())
        })?;

        let mut command = Command::new(prolog_executable);

        // Set arguments for running MQI
        command.arg("mqi");

        // --- Determine Effective Connection Details & Args ---
        let generated_password = false;
        if self.effective_password.is_none() {
             #[cfg(feature = "password-gen")]
             {
                self.effective_password = Some(Uuid::new_v4().to_string());
                debug!("Generated temporary password.");
             }
             #[cfg(not(feature = "password-gen"))]
             return Err(PrologError::FeatureNotEnabled("Password generation requires the 'password-gen' feature, or provide a password explicitly.".to_string()));
        }
        command.arg(format!("--password={}", self.effective_password.as_ref().unwrap()));

        let create_uds = false;
        if let Some(_uds_path_config) = &self.config.unix_domain_socket {
             #[cfg(all(unix, feature="unix-socket"))]
             {
                if _uds_path_config.as_os_str().is_empty() {
                    // Generate UDS path
                    let template = "/tmp/swiplrs-XXXXXX"; // Template for mkdtemp
                    let temp_dir_path = mkdtemp(template).map_err(|e| PrologError::Io(io::Error::new(io::ErrorKind::Other, format!("Failed to create temp dir for UDS: {}", e))))?;
                    // Set permissions to 700 (rwx------)
                    fs::set_permissions(&temp_dir_path, fs::Permissions::from_mode(0o700))?;

                    let socket_file_name = format!("swiplrs-{}.sock", Uuid::new_v4().to_simple());
                    let full_socket_path = temp_dir_path.join(socket_file_name);

                    // Check length constraint (92 bytes including null for portability, be conservative)
                    if full_socket_path.as_os_str().len() > 80 {
                        // Clean up directory before erroring
                        let _ = fs::remove_dir_all(&temp_dir_path);
                        return Err(PrologError::InvalidState("Generated UDS path is too long".to_string()));
                    }

                    self.effective_uds_path = Some(full_socket_path);
                    self.generated_uds_dir = Some(temp_dir_path); // Store dir for cleanup
                    create_uds = true;
                    command.arg("--create_unix_domain_socket=true");
                    debug!("Generated UDS path: {:?}", self.effective_uds_path.as_ref().unwrap());
                } else {
                    // Use provided path
                    self.effective_uds_path = Some(_uds_path_config.clone());
                    command.arg(format!("--unix_domain_socket={}", create_prolog_path(_uds_path_config)?));
                }
             }
             #[cfg(not(all(unix, feature="unix-socket")))]
             return Err(PrologError::FeatureNotEnabled("unix-socket feature required".into()));
        } else {
            // Using TCP
            if let Some(port) = self.config.port {
                command.arg(format!("--port={}", port));
            }
            // If port is None, Prolog will pick one.
        }

        // --- Add Other Config Args ---
        if let Some(count) = self.config.pending_connection_count {
            command.arg(format!("--pending_connections={}", count));
        }
        if let Some(timeout) = self.config.query_timeout_seconds {
            command.arg(format!("--query_timeout={}", timeout));
        }
        if let Some(file) = &self.config.output_file_name {
            command.arg(format!("--write_output_to_file={}", create_prolog_path(file)?));
        }
        if let Some(extra_args) = &self.config.prolog_path_args {
            command.args(extra_args);
        }

        // --- Spawn Process ---
        let args_for_debug: Vec<String> = command.get_args()
                                                  .map(|s| s.to_string_lossy().into_owned())
                                                  .collect();
        debug!("Spawning: {:?} {}", prolog_executable, args_for_debug.join(" "));
        command.stdin(Stdio::null()); // Don't need stdin
        command.stdout(Stdio::piped());
        command.stderr(Stdio::null()); // Redirect stderr to null to prevent potential deadlocks

        let mut child = command.spawn().map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                PrologError::LaunchError("'swipl' executable not found in PATH. Please ensure SWI-Prolog is installed and accessible.".to_string())
            } else {
                PrologError::LaunchError(format!("Failed to spawn swipl process: {}", e))
            }
        })?;

        let child_stdout = child.stdout.take().ok_or_else(|| PrologError::LaunchError("Failed to capture swipl stdout".to_string()))?;
        let process_id = child.id();
        info!("SWI-Prolog process started (PID: {}).", process_id);
        self.process = Some(child); // Store child handle

        // === DIAGNOSTIC: Add a small delay before reading stdout ===
        debug!("[START] Waiting 100ms before reading stdout...");
        std::thread::sleep(std::time::Duration::from_millis(100));
        debug!("[START] Delay finished. Proceeding to read stdout...");
        // ==========================================================

        // --- Read Connection Details from Stdout ---
        debug!("[START] Reading connection details from stdout...");
        let mut reader = BufReader::new(child_stdout);
        // --- Read first line (port or UDS path) robustly ---
        let mut line1_bytes = Vec::new();
        let mut raw_line1_bytes = Vec::new();
        let mut byte_buf = [0; 1];
        loop {
            reader.read_exact(&mut byte_buf)?;
            raw_line1_bytes.push(byte_buf[0]);
            if byte_buf[0] == b'\n' { // Stop at the first newline
                if let Some(&b'\r') = raw_line1_bytes.get(raw_line1_bytes.len().saturating_sub(2)) {
                    // If ended with CRLF, pop both
                    line1_bytes.truncate(line1_bytes.len().saturating_sub(1)); 
                }
                break;
            } else if byte_buf[0] != b'\r' { // Add byte if it's not CR
                line1_bytes.push(byte_buf[0]);
            }
        }
        let line1 = String::from_utf8(line1_bytes).map_err(|e| {
            error!("[START] Line 1 bytes are not valid UTF-8: {:02X?}, error: {}", raw_line1_bytes, e);
            PrologError::Io(io::Error::new(io::ErrorKind::InvalidData, e))
        })?;
        debug!("[START] Raw bytes line 1: {:02X?}", raw_line1_bytes);
        debug!("[START] Parsed line 1: '{}'", line1);

        // --- Read second line (password) robustly ---
        let mut line2_bytes = Vec::new();
        let mut raw_line2_bytes = Vec::new();
        loop {
            reader.read_exact(&mut byte_buf)?;
            raw_line2_bytes.push(byte_buf[0]);
             if byte_buf[0] == b'\n' { // Stop at the first newline
                if let Some(&b'\r') = raw_line2_bytes.get(raw_line2_bytes.len().saturating_sub(2)) {
                    // If ended with CRLF, pop both
                    line2_bytes.truncate(line2_bytes.len().saturating_sub(1));
                }
                break;
            } else if byte_buf[0] != b'\r' { // Add byte if it's not CR
                line2_bytes.push(byte_buf[0]);
            }
        }
        let line2 = String::from_utf8(line2_bytes).map_err(|e| {
             error!("[START] Line 2 bytes are not valid UTF-8: {:02X?}, error: {}", raw_line2_bytes, e);
             PrologError::Io(io::Error::new(io::ErrorKind::InvalidData, e))
        })?;
        debug!("[START] Raw bytes line 2: {:02X?}", raw_line2_bytes);
        debug!("[START] Parsed line 2: '{}'", line2);

        // Original trim logic might still be useful if prolog adds extra whitespace within the line?
        // let conn_detail = line1.trim();
        // let password_from_prolog = line2.trim();
        // For now, use the direct parsed strings
        let conn_detail = line1;
        let password_from_prolog = line2;

        debug!("[START] Final password read: '{}'", password_from_prolog);

        // Verify/Store Connection Details
        if self.effective_uds_path.is_some() {
            // Expect UDS path on first line
            if self.effective_uds_path.as_ref().unwrap().to_str() != Some(conn_detail.as_str()) && create_uds {
                 warn!("Generated UDS path mismatch: expected {:?}, got {}", self.effective_uds_path.as_ref().unwrap(), conn_detail);
                 // Overwrite with what Prolog actually created/used if we generated it
                 self.effective_uds_path = Some(PathBuf::from(conn_detail.clone()));
            } else if self.effective_uds_path.as_ref().unwrap().to_str() != Some(conn_detail.as_str()) {
                 return Err(PrologError::LaunchError(format!("UDS path mismatch: expected {:?}, got {}", self.effective_uds_path.as_ref().unwrap(), conn_detail)));
            }
            debug!("Using UDS path: {}", conn_detail);
        } else {
            // Expect Port on first line
            let port: u16 = conn_detail.parse().map_err(|_| PrologError::LaunchError(format!("Failed to parse port number from swipl stdout: {}", conn_detail)))?;
            if let Some(expected_port) = self.config.port {
                if expected_port != port {
                    return Err(PrologError::LaunchError(format!("Port mismatch: expected {}, got {}", expected_port, port)));
                }
            } else {
                 self.effective_port = Some(port); // Store the port Prolog picked
            }
            debug!("Using TCP port: {}", port);
        }

        // Verify/Store Password
        if let Some(expected_password) = self.effective_password.as_ref() {
            if *expected_password != password_from_prolog {
                 // Should only happen if user provided password differs from what prolog output (which shouldn't happen)
                  return Err(PrologError::LaunchError(format!("Password mismatch: expected {}, got {}", expected_password, password_from_prolog)));
            }
        } else if generated_password {
            // Should not happen if feature enabled, means generation failed silently
             return Err(PrologError::LaunchError("Password was supposed to be generated but is missing.".into()));
        } else {
            // Should only happen if feature disabled and password wasn't provided
            // This case is caught in `new`, but check again.
            return Err(PrologError::LaunchError("Password required but not available.".into()));
        }
        debug!("Confirmed password.");

        // --- Spawn Output Readers --- 
        // Stderr reader is no longer needed as stderr is redirected to null
        // Temporarily comment out to debug hang
        /*
        // Spawn thread for stderr
        thread::Builder::new().name(format!("swipl-{}-stderr", process_id)).spawn(move || {
            let stderr_reader = BufReader::new(child_stderr);
            for line in stderr_reader.lines() {
                match line {
                    Ok(l) => warn!("Prolog stderr [{}]: {}", process_id, l),
                    Err(e) => error!("Error reading Prolog stderr [{}]: {}", process_id, e),
                }
            }
            debug!("Prolog stderr thread finished for PID {}", process_id);
        }).map_err(|e| PrologError::LaunchError(format!("Failed to spawn stderr reader thread: {}", e)))?;
        */
        // Spawn thread for remaining stdout (after connection details)
        thread::Builder::new().name(format!("swipl-{}-stdout", process_id)).spawn(move || {
            // The reader now owns the stdout handle
            for line in reader.lines() {
                 match line {
                    Ok(l) => info!("Prolog stdout [{}]: {}", process_id, l),
                    Err(e) => error!("Error reading Prolog stdout [{}]: {}", process_id, e),
                }
            }
            debug!("Prolog stdout thread finished for PID {}", process_id);
        }).map_err(|e| PrologError::LaunchError(format!("Failed to spawn stdout reader thread: {}", e)))?;

        // --- Optional: Set MQI Traces ---
        // Clone traces *before* the mutable borrow for connect
        let traces_to_set = self.config.mqi_traces.clone();

        if let Some(traces) = traces_to_set { // Use the cloned value
            info!("Setting MQI traces to: {}", traces);
            // Need to create a temporary session to send the debug command
            match self.connect() { // Mutable borrow happens here
                Ok(mut temp_session) => {
                    let trace_goal = format!("debug(mqi({})).", traces); // Use original `traces` from pattern matching
                    if let Err(e) = temp_session.query(&trace_goal, None) {
                        warn!("Failed to set MQI traces via query: {}", e);
                        // Don't fail the whole start for this, just log it.
                    }
                    // Close the temporary session
                    let _ = temp_session.close();
                }
                Err(e) => {
                    error!("Failed to connect to set MQI traces: {}", e);
                    // If we can't connect immediately, something is wrong, fail start.
                    let _ = self.stop(true); // Attempt cleanup (immutable borrow `self.config` ended)
                    return Err(e);
                }
            }
        }

        Ok(())
    }

    /// Creates a new session (connection) to the MQI server.
    /// This will implicitly call `start()` if the server hasn't been started yet.
    pub fn connect(&mut self) -> Result<PrologSession, PrologError> {
        if self.process.is_none() && self.config.launch_mqi {
            debug!("Server not started, calling start() before connect().");
            self.start()?;
        }

        // Ensure we have connection details
        let password = self.effective_password.clone().ok_or_else(|| PrologError::InvalidState("Password not available for connection".to_string()))?;

        let address = self.effective_uds_path.as_ref()
            .map(|_p| {
                 #[cfg(feature = "unix-socket")]
                 { Ok(ConnectionAddr::Uds(_p.clone())) }
                 #[cfg(not(feature = "unix-socket"))]
                 { Err(PrologError::FeatureNotEnabled("unix-socket".to_string())) }
            })
            .or_else(|| self.effective_port.map(|p| Ok(ConnectionAddr::Tcp(p))))
            .ok_or_else(|| PrologError::InvalidState("No valid connection address (port/UDS) available".to_string()))??;

        PrologSession::connect(address, &password, self.connection_failed.clone())
    }

    /// Stops the SWI-Prolog process if it was launched by this instance.
    ///
    /// # Arguments
    /// * `kill` - If true, forcefully kills the process immediately. If false,
    ///   attempts a graceful shutdown by sending the `quit.` command first.
    pub fn stop(&mut self, kill: bool) -> Result<(), PrologError> {
        let uds_dir_to_clean = self.generated_uds_dir.take(); // Take ownership
        let result;

        if let Some(mut child) = self.process.take() {
            let pid = child.id();
            info!("Stopping SWI-Prolog process (PID: {})...", pid);
            let conn_failed = *self.connection_failed.lock().unwrap();

            if !kill && !conn_failed {
                debug!("Attempting graceful shutdown for PID {}...", pid);
                // Try graceful shutdown
                match self.connect() { // Need a temporary session
                    Ok(mut session) => {
                        match session.halt_server_internal() {
                            Ok(_) => info!("Sent quit command successfully to PID {}.", pid),
                            Err(e) => warn!("Failed to send quit command gracefully to PID {}: {}. Will kill.", pid, e),
                        }
                        // Close the session used for halting
                        let _ = session.close();
                    }
                    Err(e) => {
                        warn!("Failed to connect for graceful shutdown of PID {}: {}. Will kill.", pid, e);
                    }
                }
                // Give it a moment to exit after sending quit?
                // std::thread::sleep(std::time::Duration::from_millis(100));
            }

            // Kill if forced, failed graceful, or connection was already known to be bad
            debug!("Ensuring process PID {} is terminated.", pid);
            match child.kill() {
                Ok(_) => info!("Kill signal sent to SWI-Prolog process PID {}.", pid),
                Err(e) if e.kind() == io::ErrorKind::InvalidInput => {
                    // This means the process already exited, which is fine.
                    info!("SWI-Prolog process PID {} likely already exited.", pid)
                }
                Err(e) => {
                    error!("Failed to send kill signal to SWI-Prolog process PID {}: {}", pid, e);
                    // Put it back if killing failed? Maybe it can be stopped later?
                    self.process = Some(child);
                    result = Err(PrologError::Io(e));
                    // Don't clean up UDS dir if we failed to stop the process
                    self.generated_uds_dir = uds_dir_to_clean;
                    return result;
                }
            }

            match child.wait() {
                Ok(status) => info!("SWI-Prolog process PID {} exited with status: {}", pid, status),
                Err(e) => error!("Failed to wait for SWI-Prolog process PID {} to exit: {}", pid, e),
            }
            result = Ok(());

        } else {
            debug!("stop() called but no process was running (or not launched by us).");
            result = Ok(());
        }

        // Clean up generated UDS directory *after* process is confirmed stopped
        if let Some(_dir_path) = uds_dir_to_clean {
             #[cfg(all(unix, feature="unix-socket"))]
             {
                debug!("Cleaning up generated UDS directory: {:?}", _dir_path);
                if let Err(e) = fs::remove_dir_all(&_dir_path) {
                    warn!("Failed to remove generated UDS directory {:?}: {}", _dir_path, e);
                }
             }
        }

        result
    }
}

// Implement Drop to ensure the process is stopped
impl Drop for PrologServer {
    fn drop(&mut self) {
        if self.process.is_some() {
            warn!("PrologServer dropped without explicit stop(), killing process PID {}.", self.process.as_ref().map(|p| p.id()).unwrap_or(0));
            if let Err(e) = self.stop(true) {
                error!("Error stopping Prolog process during drop: {}", e);
            }
        }
         // Ensure cleanup happens even if stop wasn't called explicitly
         if let Some(_dir_path) = self.generated_uds_dir.take() {
             #[cfg(all(unix, feature="unix-socket"))]
             {
                 warn!("Cleaning up generated UDS directory {:?} during drop", _dir_path);
                 if let Err(e) = fs::remove_dir_all(&_dir_path) {
                    error!("Failed to remove generated UDS directory {:?} during drop: {}", _dir_path, e);
                }
             }
        }
    }
}

// Helper function for OS path to Prolog POSIX path
fn create_prolog_path(path: &PathBuf) -> Result<String, PrologError> {
     // Basic implementation: just return the path as a string.
     // SWI-Prolog often handles native paths reasonably well, but full
     // conversion (like Python's) might be needed for edge cases or Windows drives.
     // For Windows: C:\path -> /c/path might be needed for some predicates.
    path.to_str().map(|s| s.to_string()).ok_or_else(|| PrologError::InvalidState(format!("Path contains invalid UTF-8: {:?}", path)))
} 