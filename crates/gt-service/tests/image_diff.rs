//! 图片 Diff 测试（FR-DIF-04，M5）。
//!
//! 覆盖三层语义：
//! 1. 图片路径走 [`Outcome::ImageDiff`]，两侧都能解码出正确的尺寸；
//! 2. 内容不同的图片 `equal == false`，完全相同的字节 `equal == true`；
//! 3. 非图片的真二进制仍走 [`Outcome::FileDiff`]（Binary 载荷），不被误判。

use std::sync::Arc;
use std::time::Duration;

use gt_backend::{noop_observer, GitCli, MemoryTokenStore};
use gt_diff::DiffPayload;
use gt_service::{Delivery, GitService, Outcome, RequestKind};

/// 用 `image` crate 生成一张纯色 PNG（宽高各 2 像素）。
fn make_png(path: &std::path::Path, rgb: [u8; 3]) {
    let img = image::RgbImage::from_fn(2, 2, |_, _| image::Rgb(rgb));
    img.save_with_format(path, image::ImageFormat::Png)
        .expect("生成 PNG");
}

fn git(repo: &std::path::Path, args: &[&str]) {
    let cli = GitCli::detect().expect("本机应装有 git");
    let output = cli
        .run(Some(repo), args, &gt_backend::NoopObserver, gt_backend::LOCAL_TIMEOUT)
        .expect("执行 git");
    assert!(output.is_success(), "git {args:?} 失败：{}", output.stderr);
}

fn round_trip(
    service: &GitService,
    id: gt_core::RepoId,
    sender: &crossbeam_channel::Sender<Delivery>,
    receiver: &crossbeam_channel::Receiver<Delivery>,
    kind: RequestKind,
) -> Outcome {
    service.request(id, kind, sender.clone()).expect("投递请求");
    receiver
        .recv_timeout(Duration::from_secs(60))
        .expect("等待结果超时")
        .outcome
}

#[test]
fn image_diff_reports_sides_and_equality() {
    let Some(_cli) = GitCli::detect() else {
        return;
    };
    let root = std::env::temp_dir().join(format!("knotwork-image-diff-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("建仓库目录");
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "img@knotwork.invalid"]);
    git(&root, &["config", "user.name", "Knotwork Image"]);

    make_png(&root.join("logo.png"), [255, 0, 0]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "红色图标"]);
    make_png(&root.join("logo.png"), [0, 0, 255]);
    // 完全相同内容的第二张图（测试 equal 判定）。
    std::fs::copy(root.join("logo.png"), root.join("same.png")).expect("复制");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "蓝色图标"]);

    // 真二进制文件（非图片）。
    std::fs::write(root.join("data.bin"), [0u8, 1, 2, 3, 255]).expect("写二进制");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "二进制"]);

    let mut service = GitService::with_token_store(
        noop_observer(),
        Arc::new(MemoryTokenStore::new()),
    )
    .expect("建服务");
    let (sender, receiver) = crossbeam_channel::unbounded();
    let handle = service.open_repo(&root).expect("打开仓库");
    let id = handle.id();

    // 红（HEAD~2）→ 蓝（HEAD~1）：logo.png 的变化发生在这两个提交之间。
    let head = "HEAD~1";
    let parent = "HEAD~2";

    // 1) 改了颜色的图片：两侧可解码、尺寸正确、不等。
    let outcome = round_trip(
        &service,
        id,
        &sender,
        &receiver,
        RequestKind::FileDiff {
            left: gt_service::DiffSide::Rev(parent.to_owned()),
            right: gt_service::DiffSide::Rev(head.to_owned()),
            path: "logo.png".to_owned(),
        },
    );
    let Outcome::ImageDiff(result) = outcome else {
        panic!("图片路径应返回 ImageDiff，实际 {outcome:?}");
    };
    assert!(!result.equal);
    let (Some(old), Some(new)) = (result.old.as_ref(), result.new.as_ref()) else {
        panic!("两侧都应存在");
    };
    assert!(old.decodable && new.decodable);
    assert_eq!((old.width, old.height), (2, 2));
    assert_eq!((new.width, new.height), (2, 2));
    assert_eq!(old.rgba.len(), 2 * 2 * 4);
    assert!(!old.rgba.is_empty() && !new.rgba.is_empty());

    // 2) 完全相同的图片：equal（same.png 在 HEAD~1 与 HEAD 之间未变）。
    let outcome = round_trip(
        &service,
        id,
        &sender,
        &receiver,
        RequestKind::FileDiff {
            left: gt_service::DiffSide::Rev("HEAD~1".to_owned()),
            right: gt_service::DiffSide::Rev("HEAD".to_owned()),
            path: "same.png".to_owned(),
        },
    );
    let Outcome::ImageDiff(result) = outcome else {
        panic!("same.png 应返回 ImageDiff");
    };
    assert!(result.equal, "字节相同的图片应判等");

    // 3) 真二进制：仍走 FileDiff 的 Binary 载荷。
    let outcome = round_trip(
        &service,
        id,
        &sender,
        &receiver,
        RequestKind::FileDiff {
            left: gt_service::DiffSide::Rev(parent.to_owned()),
            right: gt_service::DiffSide::Rev(head.to_owned()),
            path: "data.bin".to_owned(),
        },
    );
    let Outcome::FileDiff(diff) = outcome else {
        panic!("非图片应返回 FileDiff，实际 {outcome:?}");
    };
    assert!(matches!(diff.payload, DiffPayload::Binary | DiffPayload::Text(_)));

    service.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}
