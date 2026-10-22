#!/bin/bash

DB_PATH="fileindex.db"

# Create database schema
sqlite3 "$DB_PATH" << EOF
CREATE TABLE files (
   id INTEGER PRIMARY KEY AUTOINCREMENT,
   path TEXT NOT NULL UNIQUE,
   filename TEXT NOT NULL,
   size INTEGER NOT NULL,
   modified_date TEXT NOT NULL,
   access_date TEXT NOT NULL, 
   birth_date TEXT,
   checksum TEXT NOT NULL,
   extension TEXT,
   directory TEXT,
   created_at TEXT DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_path ON files(path);
CREATE INDEX idx_size ON files(size);
CREATE INDEX idx_filename ON files(filename);
CREATE INDEX idx_checksum ON files(checksum);
CREATE INDEX idx_extension ON files(extension);
CREATE INDEX idx_directory ON files(directory);
CREATE INDEX idx_access_date ON files(access_date);
CREATE INDEX idx_modified_date ON files(modified_date);
EOF

echo "Database created at $DB_PATH with indexes"
