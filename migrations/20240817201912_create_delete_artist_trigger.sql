CREATE TRIGGER IF NOT EXISTS delete_artist_trigger AFTER DELETE ON album
BEGIN
    DELETE FROM artist
    WHERE artist.id = OLD.artist_id
    AND NOT EXISTS (
        SELECT 1
        FROM album
        WHERE album.artist_id = OLD.artist_id
    );
END;
