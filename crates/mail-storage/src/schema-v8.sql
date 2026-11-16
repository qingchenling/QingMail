-- 已下载正文的附件数量；NULL 表示尚未下载/未知，搜索时不计入“有附件”。
ALTER TABLE messages ADD COLUMN has_attachments INTEGER CHECK(has_attachments IS NULL OR has_attachments >= 0);
