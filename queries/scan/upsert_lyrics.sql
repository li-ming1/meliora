INSERT INTO lyrics (track_id, content)
VALUES ($1, $2)
ON CONFLICT (track_id) DO UPDATE SET content = EXCLUDED.content;
