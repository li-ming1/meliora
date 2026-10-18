/*
 Phase 1 of M3U entry resolution: the exact-path lookup, alone, so it can use
 the UNIQUE index on `track.location`.

 This exists because `lookup_track.sql` puts the exact match in the same
 top-level OR as a leading-wildcard `LIKE` (`%<file stem>%`), which no index
 can serve — SQLite then abandons indexing for the whole predicate and plans
 `SCAN t` (verified with EXPLAIN QUERY PLAN). Per M3U line that is a full
 track-table scan: with the 100k scale probe's calibration (`list_all_tracks`
 = 283 ms) a 2000-line import costs hundreds of seconds and pins all three
 pool connections, starving the row prefetch that the UI thread waits on.

 Exact match was already the first disjunct of the original OR, so resolving
 here cannot disagree with the fuzzy query for any entry it hits.

 Parameters:
    1. The exact location of the track.
 */

SELECT t.id FROM track t
    WHERE t.location = $1
