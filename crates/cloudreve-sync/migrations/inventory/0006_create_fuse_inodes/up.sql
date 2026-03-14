CREATE TABLE IF NOT EXISTS fuse_inodes (
    inode INTEGER PRIMARY KEY,
    parent_inode INTEGER NOT NULL,
    name TEXT NOT NULL,
    is_directory BOOLEAN NOT NULL DEFAULT 0,
    size BIGINT NOT NULL DEFAULT 0,
    etag TEXT NOT NULL DEFAULT '',
    created_at BIGINT NOT NULL,
    modified_at BIGINT NOT NULL,
    accessed_at BIGINT NOT NULL,
    cache_state TEXT NOT NULL DEFAULT 'not_cached',
    pinned BOOLEAN NOT NULL DEFAULT 0,
    has_error BOOLEAN NOT NULL DEFAULT 0,
    populated BOOLEAN NOT NULL DEFAULT 0,
    drive_id TEXT NOT NULL,
    UNIQUE(parent_inode, name)
);
CREATE INDEX idx_fuse_parent ON fuse_inodes(parent_inode);
CREATE INDEX idx_fuse_drive ON fuse_inodes(drive_id);
CREATE INDEX idx_fuse_cache_state ON fuse_inodes(cache_state);
