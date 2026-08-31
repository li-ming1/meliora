CREATE TRIGGER IF NOT EXISTS delete_album_trigger AFTER DELETE ON track
BEGIN
    DELETE FROM album
    WHERE album.id = OLD.album_id
    AND NOT EXISTS (
        SELECT 1
        FROM track
        WHERE track.album_id = OLD.album_id
    );
END;
