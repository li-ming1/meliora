INSERT INTO artist (name, name_sortable)
    VALUES ($1, $2)
    ON CONFLICT (name) DO NOTHING -- this means RETURNING id doesn't return anything if the artist already exists
    RETURNING id;                 -- this really sucks but updating each artist's name is an expensive operation
