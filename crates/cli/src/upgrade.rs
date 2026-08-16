//! `upgrade` 子命令：停服（释放 exe 锁）→ `rtk cargo build --release` → 重装启动。
//!
//! ## 这是 ADR-0001 决策 4（CLI 不做提权控制）的**明确例外**
//!
//! 决策 6 把升级收进同一条命令入口（`find-stutter upgrade [--no-build]`），
//! 废弃本地 `upgrade-service.ps1` 脚本（ps1 被 .gitignore 忽略在仓库外，
//! 且三次踩坑：MSYS 路径、`sc` 别名、引号转义）。因此本模块包含两条提权
//! 路径（stop / install-start），是 CLI 中唯一做提权控制的地方。
//!
//! ## 步骤编排（与旧 ps1 等价，进入版本控制）
//!
//! 1. **停服**：提权 spawn `find-stutter-service.exe stop`（UAC 系统弹出），
//!    释放 `find-stutter-service.exe` 文件锁（否则 release 构建报 os error 5）。
//! 2. **构建**（除非 `--no-build`）：调用 `rtk cargo build --release`。
//!    **必须走 rtk**（项目 AGENTS.md 硬性约定）：先在 PATH 里找 `rtk`，
//!    找不到再回退 `D:\app\cargo\bin\rtk.exe`。
//! 3. **重装启动**：提权 spawn `find-stutter-service.exe install-start`
//!    （install 内部检测 binary 变化则 change_config + 重启，首次则注册并启动）。
//! 4. **校验**：spawn `find-stutter-service.exe status`（普通权限即可），
//!    退出码 0 = Running。
//!
//! ## 布局发现
//!
//! - **仓库根**：从当前 exe 向上逐级找含 `[workspace]` 的 `Cargo.toml`
//!   （`target/debug` → `target` → 仓库根）；找不到回退 CWD。
//! - **service exe**：与 `crates/ui/src/auto_start.rs` 相同的候选顺序——
//!   exe 同目录 / CWD / PATH。

use std::path::PathBuf;
use std::time::Duration;

use crate::elevate::{self, ElevateOutcome};

/// 提权子进程的最长等待时间（停服 / install-start 通常数秒，30s 留足裕度）
const ELEVATE_TIMEOUT: Duration = Duration::from_secs(30);

/// 升级流程中的单个步骤（可枚举、可单测，不真正执行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeStep {
    /// 提权停服（释放 exe 锁）
    StopService,
    /// rtk cargo build --release
    BuildRelease,
    /// 提权重装并启动（install-start）
    InstallStart,
    /// 校验服务回到 Running
    VerifyStatus,
}

impl UpgradeStep {
    /// 人类可读的步骤描述（进度输出用）。
    pub fn describe(&self) -> &'static str {
        match self {
            UpgradeStep::StopService => "停服（UAC 提权：find-stutter-service stop）",
            UpgradeStep::BuildRelease => "构建（rtk cargo build --release）",
            UpgradeStep::InstallStart => "重装启动（UAC 提权：find-stutter-service install-start）",
            UpgradeStep::VerifyStatus => "校验（find-stutter-service status 退出码 0 = Running）",
        }
    }
}

/// 一次升级的完整计划（findings + 步骤序列）。
#[derive(Debug, Clone)]
pub struct UpgradePlan {
    pub service_exe: PathBuf,
    pub rtk: PathBuf,
    pub repo_root: PathBuf,
    pub steps: Vec<UpgradeStep>,
}

/// 在 PATH 中查找 `rtk`，找不到回退 `D:\app\cargo\bin\rtk.exe`（都不存在返回 None）。
///
/// 项目约定所有 cargo 命令必须经 rtk 执行（AGENTS.md），所以这里**不直接调 cargo**。
pub fn find_rtk() -> Option<PathBuf> {
    // 1) PATH 查找（where / which）
    let which = if cfg!(windows) { "where" } else { "which" };
    if let Ok(out) = std::process::Command::new(which).arg("rtk").output() {
        if out.status.success() {
            if let Some(line) = String::from_utf8_lossy(&out.stdout).lines().next() {
                let p = PathBuf::from(line.trim());
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    // 2) 固定回退路径（本机部署位置）
    let fallback = PathBuf::from(r"D:\app\cargo\bin\rtk.exe");
    if fallback.is_file() {
        Some(fallback)
    } else {
        None
    }
}

/// 从当前 exe 向上逐级找仓库根（含 `[workspace]` 的 Cargo.toml 目录）；找不到回退 CWD。
pub fn find_repo_root() -> PathBuf {
    if let Ok(me) = std::env::current_exe() {
        for dir in me.ancestors() {
            let manifest = dir.join("Cargo.toml");
            if manifest.is_file() {
                if let Ok(content) = std::fs::read_to_string(&manifest) {
                    if content.contains("[workspace]") {
                        return dir.to_path_buf();
                    }
                }
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// 找 `find-stutter-service.exe`：exe 同目录 / CWD / PATH（与 ui::auto_start 同序）。
pub fn find_service_exe() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            candidates.push(dir.join("find-stutter-service.exe"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("find-stutter-service.exe"));
    }
    let which = if cfg!(windows) { "where" } else { "which" };
    if let Ok(out) = std::process::Command::new(which).arg("find-stutter-service").output() {
        if out.status.success() {
            if let Some(line) = String::from_utf8_lossy(&out.stdout).lines().next() {
                candidates.push(PathBuf::from(line.trim()));
            }
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// 生成升级计划（纯函数，便于单测编排逻辑而不实际执行）。
///
/// - `no_build = true` → 跳过 BuildRelease 步骤（用已有 release exe 重装）。
/// - `rtk_override` / `service_exe_override`：测试 / 特殊部署注入；
///   None 时走真实查找（找不到 rtk 报错、找不到 service exe 报错）。
pub fn plan_upgrade(
    no_build: bool,
    rtk_override: Option<PathBuf>,
    service_exe_override: Option<PathBuf>,
    repo_root_override: Option<PathBuf>,
) -> anyhow::Result<UpgradePlan> {
    let rtk = rtk_override
        .or_else(find_rtk)
        .ok_or_else(|| anyhow::anyhow!(
            "找不到 rtk：PATH 中无 rtk，回退位置 D:\\app\\cargo\\bin\\rtk.exe 也不存在。\
             请检查 PATH 或 rtk 安装位置后重试（本项目要求所有 cargo 命令必须经 rtk 执行，见 AGENTS.md）"
        ))?;
    let service_exe = service_exe_override
        .or_else(find_service_exe)
        .ok_or_else(|| anyhow::anyhow!("找不到 find-stutter-service.exe（exe 同目录 / CWD / PATH 均无；请先构建）"))?;
    let repo_root = repo_root_override.unwrap_or_else(find_repo_root);

    let mut steps = vec![UpgradeStep::StopService];
    if !no_build {
        steps.push(UpgradeStep::BuildRelease);
    }
    steps.push(UpgradeStep::InstallStart);
    steps.push(UpgradeStep::VerifyStatus);

    Ok(UpgradePlan {
        service_exe,
        rtk,
        repo_root,
        steps,
    })
}

/// 执行升级计划（真实提权 + 构建；由 `find-stutter upgrade` 用户显式触发）。
///
/// 返回 `Ok(true)` = 全部步骤成功、服务回到 Running。
/// 每步开始前向 stderr 打印进度（stdout 保持干净，便于脚本化）。
pub fn run_upgrade(plan: &UpgradePlan) -> anyhow::Result<bool> {
    for step in &plan.steps {
        eprintln!("[upgrade] {}", step.describe());
    }
    for step in &plan.steps {
        match step {
            UpgradeStep::StopService => {
                let out = elevate::spawn_elevated_and_wait(
                    &plan.service_exe,
                    &["stop"],
                    ELEVATE_TIMEOUT,
                );
                eprintln!("[upgrade] stop 提权结果: {}", out.message());
                match out {
                    ElevateOutcome::Ok(_) => {} // stop 对「已停」也返回非 0，不视为失败
                    ElevateOutcome::UacDenied => {
                        anyhow::bail!("UAC 被拒绝，升级中止（服务未停止）");
                    }
                    other => {
                        // 停服失败但进程可能已退出（如服务本来就停着）：继续走构建，
                        // 构建若因文件锁失败会在此报出真正原因。
                        eprintln!("[upgrade] 停服未确认成功（{}），继续尝试构建", other.message());
                    }
                }
            }
            UpgradeStep::BuildRelease => {
                let output = std::process::Command::new(&plan.rtk)
                    .args(["cargo", "build", "--release"])
                    .current_dir(&plan.repo_root)
                    .output();
                match output {
                    Ok(out) if out.status.success() => {
                        eprintln!("[upgrade] 构建完成");
                    }
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        anyhow::bail!("构建失败（exit={:?}）：{}", out.status.code(), stderr);
                    }
                    Err(e) => {
                        anyhow::bail!("启动 rtk 失败（{}）：{}", plan.rtk.display(), e);
                    }
                }
            }
            UpgradeStep::InstallStart => {
                let out = elevate::spawn_elevated_and_wait(
                    &plan.service_exe,
                    &["install-start"],
                    ELEVATE_TIMEOUT,
                );
                eprintln!("[upgrade] install-start 提权结果: {}", out.message());
                if !out.is_ok() {
                    anyhow::bail!("重装启动失败：{}", out.message());
                }
            }
            UpgradeStep::VerifyStatus => {
                // 与 service status 的退出码协议一致：0 = Running
                let code = std::process::Command::new(&plan.service_exe)
                    .arg("status")
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .output()
                    .map(|o| o.status.code().unwrap_or(-1));
                match code {
                    Ok(0) => {
                        eprintln!("[upgrade] 校验通过：服务 Running");
                        return Ok(true);
                    }
                    Ok(c) => anyhow::bail!("校验失败：status 退出码 {}（非 Running）", c),
                    Err(e) => anyhow::bail!("校验失败：status 调用失败 {}", e),
                }
            }
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_paths() -> (PathBuf, PathBuf, PathBuf) {
        (
            PathBuf::from(r"D:\fake\rtk.exe"),
            PathBuf::from(r"D:\fake\find-stutter-service.exe"),
            PathBuf::from(r"D:\fake\repo"),
        )
    }

    #[test]
    fn plan_full_has_four_steps_in_order() {
        let (rtk, svc, root) = fake_paths();
        let plan = plan_upgrade(false, Some(rtk), Some(svc), Some(root)).unwrap();
        assert_eq!(
            plan.steps,
            vec![
                UpgradeStep::StopService,
                UpgradeStep::BuildRelease,
                UpgradeStep::InstallStart,
                UpgradeStep::VerifyStatus,
            ]
        );
    }

    #[test]
    fn plan_no_build_skips_build() {
        let (rtk, svc, root) = fake_paths();
        let plan = plan_upgrade(true, Some(rtk), Some(svc), Some(root)).unwrap();
        assert!(!plan.steps.contains(&UpgradeStep::BuildRelease));
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps.first(), Some(&UpgradeStep::StopService));
        assert_eq!(plan.steps.last(), Some(&UpgradeStep::VerifyStatus));
    }

    #[test]
    fn plan_with_overrides_is_generated_even_if_paths_missing() {
        let (_, _svc, _root) = fake_paths();
        // rtk/service_exe 覆盖时不做存在性检查（真实执行时 spawn 才失败）——
        // 计划总是可生成；找不到才报错的是「无 override 且环境也没有」的路径。
        let err = plan_upgrade(
            false,
            Some(PathBuf::from(r"D:\fake\rtk.exe")),
            Some(PathBuf::from(r"D:\__missing__\find-stutter-service.exe")),
            Some(PathBuf::from(r"D:\fake\repo")),
        );
        assert!(err.is_ok());
    }

    #[test]
    fn step_descriptions_are_chinese() {
        assert!(UpgradeStep::StopService.describe().contains("停服"));
        assert!(UpgradeStep::BuildRelease.describe().contains("rtk"));
        assert!(UpgradeStep::InstallStart.describe().contains("install-start"));
        assert!(UpgradeStep::VerifyStatus.describe().contains("校验"));
    }

    /// find_repo_root 冒烟：任何环境都应返回一个路径（回退 CWD 也算成功）。
    #[test]
    fn find_repo_root_returns_something() {
        let root = find_repo_root();
        assert!(!root.as_os_str().is_empty());
    }

    /// find_rtk 冒烟：本机应有 rtk（PATH 或 D:\app\cargo\bin）。
    #[test]
    fn find_rtk_on_this_machine() {
        // 开发机必有 rtk；即使 PATH 没有，D:\app\cargo\bin\rtk.exe 也应命中。
        // （其他环境跑不到此断言的前提是装了 rtk——本项目测试本来就要求 rtk。）
        if let Some(p) = find_rtk() {
            assert!(p.is_file(), "find_rtk 返回了不存在的路径: {}", p.display());
        }
    }
}
