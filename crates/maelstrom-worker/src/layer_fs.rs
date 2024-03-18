use anyhow::Result;
use futures::StreamExt as _;
use maelstrom_base::{manifest::UnixTimestamp, ArtifactType, Sha256Digest};
use maelstrom_fuse::{BottomLayerBuilder, LayerFs, UpperLayerBuilder};
use maelstrom_util::async_fs::Fs;
use std::path::{Path, PathBuf};

async fn dir_size(fs: &Fs, path: &Path) -> Result<u64> {
    let mut total = 0;
    let mut entries = fs.read_dir(path).await?;
    while let Some(e) = entries.next().await {
        let e = e?;
        total += e.metadata().await?.len();
    }
    Ok(total)
}

pub async fn build_bottom_layer(
    layer_path: PathBuf,
    cache_path: PathBuf,
    artifact_digest: Sha256Digest,
    _artifact_type: ArtifactType,
    artifact_path: PathBuf,
) -> Result<u64> {
    let fs = Fs::new();
    let mut builder =
        BottomLayerBuilder::new(&fs, &layer_path, &cache_path, UnixTimestamp::EPOCH).await?;
    builder
        .add_from_tar(artifact_digest, fs.open_file(artifact_path).await?)
        .await?;
    builder.finish();

    dir_size(&fs, &layer_path).await
}

pub async fn build_upper_layer(
    layer_path: PathBuf,
    cache_path: PathBuf,
    lower_layer_path: PathBuf,
    upper_layer_path: PathBuf,
) -> Result<u64> {
    let fs = Fs::new();
    let lower = LayerFs::from_path(&lower_layer_path, &cache_path).await?;
    let upper = LayerFs::from_path(&upper_layer_path, &cache_path).await?;
    let mut builder = UpperLayerBuilder::new(&layer_path, &cache_path, &lower).await?;
    builder.fill_from_bottom_layer(&upper).await?;
    builder.finish();

    dir_size(&fs, &layer_path).await
}
