-- add virtual column so we can make an index on sort date
ALTER TABLE album ADD COLUMN sort_date GENERATED ALWAYS AS (
    COALESCE(release_date, printf('%04d-01-01', release_year))
) VIRTUAL;
