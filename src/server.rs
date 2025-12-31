use tokio::process::{Command, Child, ChildStdin};
use tokio::io::{AsyncBufReadExt, BufReader, AsyncWriteExt};
use std::process::Stdio;
use tokio::sync::mpsc;
use log::{info, error};
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct ServerProcess {
    child: Option<Arc<Mutex<Child>>>,
    stdin: Option<Arc<Mutex<ChildStdin>>>,
}

#[derive(Debug)]
pub enum ServerEvent {
    Output(String), // stdout/stderr line
    Exit(Option<i32>),
}

impl ServerProcess {
    pub fn new() -> Self {
        Self { child: None, stdin: None }
    }

    pub fn is_running(&self) -> bool {
        self.child.is_some()
    }

    pub async fn start(&mut self, jar_path: &str, args: Vec<String>, event_tx: mpsc::Sender<ServerEvent>) -> anyhow::Result<()> {
        if self.is_running() {
            return Err(anyhow::anyhow!("Server is already running"));
        }

        info!("Starting server: java -jar {} {:?}", jar_path, args);

        // Parse JVM args and program args clearly
        // Expected format: java [JVM Args] -jar server.jar [Program Args]
        // The `args` valid here are MIXED, which is bad. The caller should separate them.
        // For now, let's assume `args` passed from main.rs are ONLY JVM args + "nogui".
        // BUT wait, in main.rs: args = ["-Xmx...", "-Xms...", "nogui"]
        // "nogui" is a PROGRAM arg. -Xmx is a JVM arg.
        
        let mut jvm_args = Vec::new();
        let mut prog_args = Vec::new();

        for arg in args {
            if arg.starts_with("-X") || arg.starts_with("-D") {
                jvm_args.push(arg);
            } else {
                prog_args.push(arg);
            }
        }

        info!("Starting server with JVM: {:?} and Program: {:?}", jvm_args, prog_args);

        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        // If jar_path is an absolute path, run in its parent directory so the server picks up eula.txt, world/, etc.
        let jar_path_buf = std::path::PathBuf::from(jar_path);
        let cwd = jar_path_buf.parent().map(|p| p.to_path_buf()).unwrap_or(std::path::PathBuf::from(home).join("conductor"));

        let mut child = Command::new("java")
            .args(jvm_args)
            .arg("-jar")
            .arg(jar_path)
            .args(prog_args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()) // Capture stderr too
            .spawn()?;

        let stdin = child.stdin.take().ok_or(anyhow::anyhow!("Failed to capture stdin"))?;
        let stdout = child.stdout.take().ok_or(anyhow::anyhow!("Failed to capture stdout"))?;
        let stderr = child.stderr.take().ok_or(anyhow::anyhow!("Failed to capture stderr"))?;

        // Spawn capture task for stdout
        let tx = event_tx.clone();
        let stdout_task = tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
               if let Err(_) = tx.send(ServerEvent::Output(line)).await { break; }
            }
        });

        // Spawn capture task for stderr
        let tx_err = event_tx.clone();
        let stderr_task = tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
               if let Err(_) = tx_err.send(ServerEvent::Output(line)).await { break; }
            }
        });

        // Wrap child and stdin in Arc<Mutex> so we can await/kill from other tasks
        let child_arc = Arc::new(Mutex::new(child));
        let stdin_arc = Arc::new(Mutex::new(stdin));

        // Spawn waiter to detect exit
        let tx_wait = event_tx.clone();
        let child_clone = child_arc.clone();
        tokio::spawn(async move {
            let mut guard = child_clone.lock().await;
            match guard.wait().await {
                Ok(status) => {
                    let code = status.code();
                    let _ = tx_wait.send(ServerEvent::Output(format!("Server exited with status: {:?}", code))).await;
                    let _ = tx_wait.send(ServerEvent::Exit(code)).await;
                }
                Err(e) => {
                    let _ = tx_wait.send(ServerEvent::Output(format!("Server wait error: {}", e))).await;
                    let _ = tx_wait.send(ServerEvent::Exit(None)).await;
                }
            }
        });

        self.child = Some(child_arc);
        self.stdin = Some(stdin_arc);

        Ok(())
    }

    pub async fn stop(&mut self) -> anyhow::Result<()> {
        if let Some(child_arc) = self.child.take() {
            info!("Stopping server...");
            let mut guard = child_arc.lock().await;
            guard.kill().await?;
            self.stdin = None;
            info!("Server stopped.");
        }
        Ok(())
    }

    pub async fn write_command(&mut self, cmd: &str) -> anyhow::Result<()> {
        if let Some(stdin_arc) = &self.stdin {
            let mut guard = stdin_arc.lock().await;
            guard.write_all(format!("{}\n", cmd).as_bytes()).await?;
            guard.flush().await?;
            Ok(())
        } else {
            Err(anyhow::anyhow!("Server not running (no stdin)"))
        }
    }
}
