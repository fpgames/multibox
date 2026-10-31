//! `git multibox setup`: creates a reflink-capable area on Windows.
//!
//! The area is a Dev Drive inside a new, dynamically expanding VHDX. No existing
//! volume is reformatted: the only thing written to the host disk is one new
//! file, and the generated script refuses to format anything except the virtual
//! disk backed by that file.

use crate::doctor::{supports_dev_drive, windows_build};
use crate::{Error, Result};
use multibox_clone::{Volume, volumes};
use std::path::{Path, PathBuf};

const TASK_NAME: &str = "Multibox Dev Drive";
const LETTER_PREFERENCE: &str = "VWXYZUTSRQPONMLKJIHG";
const MIN_SIZE_GIB: u64 = 50;
const DEFAULT_SIZE_GIB: u64 = 1024;

/// Where the new volume appears.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mount {
    Letter(char),
    Folder(PathBuf),
}

#[derive(Clone, Debug, Default)]
pub struct SetupOptions {
    /// Location of the new VHDX. Defaults to the fixed drive with the most free space.
    pub vhdx: Option<PathBuf>,
    pub size_gib: Option<u64>,
    pub mount: Option<Mount>,
}

#[derive(Clone, Debug)]
pub struct SetupPlan {
    pub vhdx: PathBuf,
    pub size_gib: u64,
    pub mount: Mount,
}

/// The VHDX expands on demand, so it goes on the fixed drive with the most room.
/// The system drive only wins when there is nothing else.
fn choose_host(volumes: &[Volume], system_drive: &str) -> Option<Volume> {
    let candidates = || volumes.iter().filter(|volume| volume.fixed && !volume.reflink);
    candidates()
        .filter(|volume| !volume.root.eq_ignore_ascii_case(system_drive))
        .max_by_key(|volume| volume.free)
        .or_else(|| candidates().max_by_key(|volume| volume.free))
        .cloned()
}

fn choose_letter(volumes: &[Volume]) -> Option<char> {
    LETTER_PREFERENCE.chars().find(|letter| {
        !volumes
            .iter()
            .any(|volume| volume.root.starts_with(*letter))
    })
}

/// Resolves defaults and checks prerequisites. Nothing is created.
pub fn plan_setup(options: &SetupOptions) -> Result<SetupPlan> {
    if !cfg!(windows) {
        return Err(Error::new(
            "setup is only needed on Windows: APFS, Btrfs and XFS support reflinks directly",
        ));
    }
    if let Some((build, revision)) = windows_build() {
        if !supports_dev_drive(build, revision) {
            return Err(Error::new(format!(
                "Dev Drive needs Windows 11 22H2 or later (this is build {build}.{revision}); worktrees keep using copy mode"
            )));
        }
    }

    let size_gib = options.size_gib.unwrap_or(DEFAULT_SIZE_GIB);
    if size_gib < MIN_SIZE_GIB {
        return Err(Error::new(format!(
            "a Dev Drive must be at least {MIN_SIZE_GIB} GiB"
        )));
    }

    let known = volumes();
    let vhdx = match &options.vhdx {
        Some(path) => path.clone(),
        None => {
            let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
            let host = choose_host(&known, &format!("{system}\\"))
                .ok_or_else(|| Error::new("no fixed drive found to hold the VHDX"))?;
            PathBuf::from(host.root).join("Multibox").join("multibox.vhdx")
        }
    };
    if vhdx.exists() {
        return Err(Error::new(format!(
            "{} already exists; setup never reuses or replaces an existing file",
            vhdx.display()
        )));
    }

    let mount = match &options.mount {
        Some(mount) => mount.clone(),
        None => Mount::Letter(
            choose_letter(&known).ok_or_else(|| Error::new("no free drive letter"))?,
        ),
    };
    if let Mount::Letter(letter) = mount {
        if known.iter().any(|volume| volume.root.starts_with(letter)) {
            return Err(Error::new(format!("drive {letter}: is already in use")));
        }
    }
    Ok(SetupPlan {
        vhdx,
        size_gib,
        mount,
    })
}

fn ps_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// The elevated script. Every step that touches a disk first proves that the
/// disk is backed by the file this run created.
pub fn render_script(plan: &SetupPlan, result_file: &Path) -> String {
    let (letter, folder) = match &plan.mount {
        Mount::Letter(letter) => (letter.to_string(), String::new()),
        Mount::Folder(path) => (String::new(), path.display().to_string()),
    };
    let header = format!(
        "$vhdx = {}\n$sizeMb = {}\n$letter = {}\n$folder = {}\n$resultFile = {}\n$taskName = {}\n",
        ps_quote(&plan.vhdx.display().to_string()),
        plan.size_gib * 1024,
        ps_quote(&letter),
        ps_quote(&folder),
        ps_quote(&result_file.display().to_string()),
        ps_quote(TASK_NAME),
    );
    format!("{header}{SCRIPT_BODY}")
}

const SCRIPT_BODY: &str = r#"
$ErrorActionPreference = 'Stop'
$created = $false

function Report($ok, $message) {
    @{ ok = $ok; message = $message } | ConvertTo-Json | Set-Content -LiteralPath $resultFile -Encoding UTF8
}

function Run-Diskpart($lines) {
    $script = [IO.Path]::GetTempFileName()
    $lines | Set-Content -LiteralPath $script -Encoding ASCII
    $output = & diskpart.exe /s $script 2>&1 | Out-String
    $code = $LASTEXITCODE
    Remove-Item -LiteralPath $script
    if ($code -ne 0) { throw "diskpart failed ($code): $output" }
}

try {
    if (Test-Path -LiteralPath $vhdx) { throw "$vhdx already exists" }
    $dir = Split-Path -Parent $vhdx
    New-Item -ItemType Directory -Force -Path $dir | Out-Null

    Run-Diskpart @(
        "create vdisk file=`"$vhdx`" maximum=$sizeMb type=expandable",
        "select vdisk file=`"$vhdx`"",
        'attach vdisk',
        'convert gpt',
        'create partition primary'
    )
    $created = $true

    $disk = Get-DiskImage -ImagePath $vhdx | Get-Disk
    if ($disk.BusType -ne 'File Backed Virtual' -or $disk.Location -ne $vhdx) {
        throw "disk $($disk.Number) is not backed by $vhdx; nothing was formatted"
    }
    $partition = Get-Partition -DiskNumber $disk.Number | Where-Object Type -eq 'Basic' | Select-Object -First 1
    if (-not $partition) { throw "no partition on the new virtual disk" }

    if ($letter) {
        $partition | Set-Partition -NewDriveLetter $letter
    } else {
        New-Item -ItemType Directory -Force -Path $folder | Out-Null
        if (Get-ChildItem -LiteralPath $folder -Force) { throw "$folder is not empty" }
        $partition | Add-PartitionAccessPath -AccessPath $folder
    }
    $partition = Get-Partition -DiskNumber $disk.Number -PartitionNumber $partition.PartitionNumber
    $partition | Format-Volume -DevDrive -NewFileSystemLabel 'Multibox' -Force -Confirm:$false | Out-Null

    # A VHDX does not reattach by itself after a reboot.
    $attach = Join-Path $dir 'attach.diskpart'
    @("select vdisk file=`"$vhdx`"", 'attach vdisk') | Set-Content -LiteralPath $attach -Encoding ASCII
    $action = New-ScheduledTaskAction -Execute 'diskpart.exe' -Argument "/s `"$attach`""
    $trigger = New-ScheduledTaskTrigger -AtStartup
    $principal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
    $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null

    Report $true 'ok'
} catch {
    $message = $_.Exception.Message
    # Only the file this run created is ever removed.
    if ($created) {
        try {
            Run-Diskpart @("select vdisk file=`"$vhdx`"", 'detach vdisk')
            Remove-Item -LiteralPath $vhdx
        } catch {
            $message += " (cleanup failed: $($_.Exception.Message))"
        }
    }
    Report $false $message
}
"#;

#[cfg(windows)]
pub fn run_setup(plan: &SetupPlan) -> Result<()> {
    use std::process::Command;
    let directory = std::env::temp_dir().join(format!("multibox-setup-{}", std::process::id()));
    std::fs::create_dir_all(&directory)?;
    let script = directory.join("setup.ps1");
    let result = directory.join("result.json");

    // Windows PowerShell reads a BOM-less script as the legacy code page.
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(render_script(plan, &result).as_bytes());
    std::fs::write(&script, bytes)?;

    let launcher = format!(
        "Start-Process powershell.exe -Verb RunAs -Wait -WindowStyle Hidden -ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-File',{})",
        ps_quote(&format!("\"{}\"", script.display()))
    );
    let status = Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", &launcher])
        .status()?;
    let outcome = std::fs::read_to_string(&result);
    let _ = std::fs::remove_dir_all(&directory);
    if !status.success() {
        return Err(Error::new(
            "the administrator prompt was declined; nothing was changed",
        ));
    }
    let outcome = outcome.map_err(|_| Error::new("setup finished without reporting a result"))?;
    if outcome.contains("\"ok\":  true") || outcome.contains("\"ok\": true") {
        Ok(())
    } else {
        Err(Error::new(format!("setup failed: {}", outcome.trim())))
    }
}

#[cfg(not(windows))]
pub fn run_setup(_plan: &SetupPlan) -> Result<()> {
    Err(Error::new("setup is only needed on Windows"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(root: &str, free: u64, fixed: bool, reflink: bool) -> Volume {
        Volume {
            root: root.to_owned(),
            filesystem: "NTFS".to_owned(),
            reflink,
            free,
            total: free * 2,
            fixed,
        }
    }

    #[test]
    fn prefers_the_roomiest_data_drive() {
        let volumes = [
            volume("C:\\", 900, true, false),
            volume("D:\\", 500, true, false),
            volume("E:\\", 700, true, false),
            volume("G:\\", 9000, false, false),
        ];
        assert_eq!(choose_host(&volumes, "C:\\").unwrap().root, "E:\\");
    }

    #[test]
    fn falls_back_to_the_system_drive() {
        let volumes = [volume("C:\\", 900, true, false)];
        assert_eq!(choose_host(&volumes, "C:\\").unwrap().root, "C:\\");
    }

    #[test]
    fn picks_an_unused_letter() {
        let volumes = [volume("V:\\", 1, true, false), volume("W:\\", 1, true, false)];
        assert_eq!(choose_letter(&volumes), Some('X'));
    }

    #[test]
    fn quotes_paths_for_powershell() {
        assert_eq!(ps_quote("D:\\it's\\x"), "'D:\\it''s\\x'");
    }

    #[test]
    fn script_never_formats_by_drive_letter() {
        let plan = SetupPlan {
            vhdx: PathBuf::from("D:\\Multibox\\multibox.vhdx"),
            size_gib: 100,
            mount: Mount::Letter('V'),
        };
        let script = render_script(&plan, Path::new("D:\\result.json"));
        assert!(script.contains("$sizeMb = 102400"));
        assert!(script.contains("File Backed Virtual"));
        assert!(script.contains("$partition | Format-Volume"));
        assert!(!script.contains("Format-Volume -DriveLetter"));
    }
}
