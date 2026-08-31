
ALTER TABLE album ADD COLUMN date_precision INTEGER;

-- full date: format and mark precision = 1
UPDATE album SET
    date_precision = 1,
    release_date = strftime('%Y-%m-%d', release_date)
WHERE release_date IS NOT NULL;

-- year-only, move and mark precision = 0
UPDATE album SET
    date_precision = 0,
    release_date = printf('%04d-01-01', release_year)
WHERE release_date IS NULL AND release_year IS NOT NULL;

-- every album sort sorts by this so we'll index it
CREATE INDEX IF NOT EXISTS album_release_date_idx ON album (release_date);
