use crate::ty::{
    decode_path, encode_path, AttributesId, FileAttributes, FileData, FileId, FileTableEntry,
    FileType, LayerFsVersion, LayerId,
};
use crate::LayerFs;
use anyhow::Result;
use anyhow_trace::anyhow_trace;
use maelstrom_util::async_fs::{File, Fs};
use maelstrom_util::io::BufferedStream;
use serde::{Deserialize, Serialize};
use std::io::SeekFrom;
use std::num::NonZeroU32;
use std::path::Path;
use tokio::io::{AsyncSeekExt as _, AsyncWriteExt as _};

pub struct FileMetadataReader {
    file_table: BufferedStream<File>,
    file_table_start: u64,
    attr_table: BufferedStream<File>,
    attr_table_start: u64,
    layer_id: LayerId,
}

const CHUNK_SIZE: usize = 512;
const CACHE_SIZE: usize = 64;

#[anyhow_trace]
impl FileMetadataReader {
    pub async fn new(layer_fs: &LayerFs, layer_id: LayerId) -> Result<Self> {
        let mut file_table = BufferedStream::new(
            CHUNK_SIZE,
            CACHE_SIZE.try_into().unwrap(),
            layer_fs
                .data_fs
                .open_file(layer_fs.file_table_path(layer_id).await?)
                .await?,
        )
        .await?;
        let _header: FileTableHeader = decode_path(&mut file_table).await?;
        let file_table_start = file_table.stream_position().await?;

        let mut attr_table = BufferedStream::new(
            CHUNK_SIZE,
            CACHE_SIZE.try_into().unwrap(),
            layer_fs
                .data_fs
                .open_file(layer_fs.attributes_table_path(layer_id).await?)
                .await?,
        )
        .await?;
        let _header: AttributesTableHeader = decode_path(&mut attr_table).await?;
        let attr_table_start = attr_table.stream_position().await?;
        Ok(Self {
            file_table,
            file_table_start,
            attr_table,
            attr_table_start,
            layer_id,
        })
    }

    pub async fn get_attr(&mut self, id: FileId) -> Result<(FileType, FileAttributes)> {
        assert_eq!(id.layer(), self.layer_id);

        self.file_table
            .seek(SeekFrom::Start(self.file_table_start + id.offset_u64() - 1))
            .await?;
        let entry: FileTableEntry = decode_path(&mut self.file_table).await?;

        self.attr_table
            .seek(SeekFrom::Start(
                self.attr_table_start + entry.attr_id.offset() - 1,
            ))
            .await?;
        let attrs: FileAttributes = decode_path(&mut self.attr_table).await?;

        Ok((entry.kind, attrs))
    }

    pub async fn get_data(&mut self, id: FileId) -> Result<(FileType, FileData)> {
        assert_eq!(id.layer(), self.layer_id);

        self.file_table
            .seek(SeekFrom::Start(self.file_table_start + id.offset_u64() - 1))
            .await?;
        let entry: FileTableEntry = decode_path(&mut self.file_table).await?;

        Ok((entry.kind, entry.data))
    }
}

#[derive(Copy, Clone, Default, Debug, Deserialize, Serialize)]
pub struct FileTableHeader {
    pub version: LayerFsVersion,
}

#[derive(Copy, Clone, Default, Debug, Deserialize, Serialize)]
pub struct AttributesTableHeader {
    pub version: LayerFsVersion,
}

pub struct FileMetadataWriter {
    layer_id: LayerId,
    file_table: BufferedStream<File>,
    file_table_start: u64,
    attr_table: BufferedStream<File>,
    attr_table_start: u64,
}

#[anyhow_trace]
impl FileMetadataWriter {
    pub async fn new(
        data_fs: &Fs,
        layer_id: LayerId,
        file_table_path: &Path,
        attributes_table_path: &Path,
    ) -> Result<Self> {
        let mut file_table = BufferedStream::new(
            CHUNK_SIZE,
            CACHE_SIZE.try_into().unwrap(),
            data_fs.create_file_read_write(file_table_path).await?,
        )
        .await?;
        encode_path(&mut file_table, &FileTableHeader::default()).await?;
        let file_table_start = file_table.stream_position().await?;
        let mut attr_table = BufferedStream::new(
            CHUNK_SIZE,
            CACHE_SIZE.try_into().unwrap(),
            data_fs
                .create_file_read_write(attributes_table_path)
                .await?,
        )
        .await?;
        encode_path(&mut attr_table, &AttributesTableHeader::default()).await?;
        let attr_table_start = file_table.stream_position().await?;

        Ok(Self {
            layer_id,
            file_table,
            file_table_start,
            attr_table,
            attr_table_start,
        })
    }

    pub async fn insert_file(
        &mut self,
        kind: FileType,
        attrs: FileAttributes,
        data: FileData,
    ) -> Result<FileId> {
        let attr_id = AttributesId::try_from(
            self.attr_table.stream_position().await? - self.attr_table_start + 1,
        )
        .unwrap();
        encode_path(&mut self.attr_table, &attrs).await?;

        let entry = FileTableEntry {
            kind,
            data,
            attr_id,
        };

        let offset =
            u32::try_from(self.file_table.stream_position().await? - self.file_table_start + 1)
                .unwrap();
        let file_id = FileId::new(self.layer_id, NonZeroU32::new(offset).unwrap());
        encode_path(&mut self.file_table, &entry).await?;

        Ok(file_id)
    }

    pub async fn update_attributes(&mut self, id: FileId, attrs: FileAttributes) -> Result<()> {
        let old_file_table_pos = self.file_table.stream_position().await?;
        let old_attr_table_pos = self.attr_table.stream_position().await?;

        assert_eq!(id.layer(), self.layer_id);
        self.file_table
            .seek(SeekFrom::Start(self.file_table_start + id.offset_u64() - 1))
            .await?;
        let entry: FileTableEntry = decode_path(&mut self.file_table).await?;
        self.attr_table
            .seek(SeekFrom::Start(
                self.attr_table_start + entry.attr_id.offset() - 1,
            ))
            .await?;
        encode_path(&mut self.attr_table, &attrs).await?;

        self.file_table
            .seek(SeekFrom::Start(old_file_table_pos))
            .await?;
        self.attr_table
            .seek(SeekFrom::Start(old_attr_table_pos))
            .await?;
        Ok(())
    }

    pub async fn flush(&mut self) -> Result<()> {
        self.file_table.flush().await?;
        self.attr_table.flush().await?;
        Ok(())
    }
}
