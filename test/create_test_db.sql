CREATE TABLE types_test (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    price REAL,
    quantity INTEGER,
    active BOOLEAN,
    data BLOB,
    created_at TEXT
);
INSERT INTO types_test VALUES (1, 'Widget', 9.99, 100, 1, X'DEADBEEF', '2026-01-15T10:30:00');
INSERT INTO types_test VALUES (2, 'Gadget', 24.50, NULL, 0, NULL, '2026-02-20T14:00:00');
INSERT INTO types_test VALUES (3, 'Doohickey', 0.50, 9999, 1, X'00', '2026-03-01T00:00:00');

CREATE TABLE empty_table (id INTEGER PRIMARY KEY, value TEXT);

CREATE VIEW types_view AS SELECT id, name, price FROM types_test WHERE active = 1;
