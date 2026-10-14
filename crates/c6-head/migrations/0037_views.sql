-- Views (docs/os.md §4.5) are a system collection named "views" in every
-- realm: a collection of your own by that name moves to "views_1" first,
-- records and all.
UPDATE records SET collection = 'views_1'
    WHERE collection = 'views' AND realm IN (SELECT realm FROM collections WHERE name = 'views' AND system = 0);
UPDATE collections SET name = 'views_1' WHERE name = 'views' AND system = 0;
