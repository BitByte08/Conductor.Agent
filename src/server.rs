use tokio::process::{Command, Child, ChildStdin};
use tokio::io::{AsyncBufReadExt, BufReader, AsyncWriteExt};
use std::process::Stdio;
use tokio::sync::mpsc;
use log::info;

pub struct ServerProcess {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
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

        let mut child = Command::new("java")
            .args(jvm_args)
            .arg("-jar")
            .arg(jar_path)
            .args(prog_args)
            .current_dir("minecraft") // Run in minecraft/ subdir
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()) // Capture stderr too
            .spawn()?;

        let stdin = child.stdin.take().ok_or(anyhow::anyhow!("Failed to capture stdin"))?;
        let stdout = child.stdout.take().ok_or(anyhow::anyhow!("Failed to capture stdout"))?;
        
        // Spawn capture task
        let stderr = child.stderr.take().ok_or(anyhow::anyhow!("Failed to capture stderr"))?;
        
        // Spawn capture task for stdout
        let tx = event_tx.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
               if let Err(_) = tx.send(ServerEvent::Output(line)).await { break; }
            }
        });

        // Spawn capture task for stderr
        let tx_err = event_tx.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
               if let Err(_) = tx_err.send(ServerEvent::Output(line)).await { break; }
            }
        });

        self.child = Some(child);
        self.stdin = Some(stdin);
        
        // Spawn waiter task to detect actual exit
        // Note: we can't easily wait on self.child because we need mutable access to it to kill it later.
        // For simple MVP we rely on the main loop checking wait_zero? Or just stdout closing.
        // Ideally we'd wrap child in an Arc<Mutex> or pass it to a manager task, but let's keep it simple for now.

        Ok(())
    }

    pub async fn stop(&mut self) -> anyhow::Result<()> {
        if let Some(mut child) = self.child.take() {
            info!("Stopping server...");
            // Try graceful stop first?
            // self.write_command("stop").await?; 
            // For now, just kill or send SIGTERM if we could.
            child.kill().await?; 
            self.stdin = None;
            info!("Server stopped.");
        }
        Ok(())
    }

    pub async fn write_command(&mut self, cmd: &str) -> anyhow::Result<()> {
        if let Some(stdin) = &mut self.stdin {
            stdin.write_all(format!("{}\n", cmd).as_bytes()).await?;
            stdin.flush().await?;
            Ok(())
        } else {
            Err(anyhow::anyhow!("Server not running (no stdin)"))
        }
    }
}
