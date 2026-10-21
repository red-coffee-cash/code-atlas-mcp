import sqlite3


class Database:
    def __init__(self, path):
        self.conn = sqlite3.connect(path)

    def query(self, sql, params=()):
        cur = self.conn.execute(sql, params)
        return cur.fetchall()

    def execute(self, sql, params=()):
        self.conn.execute(sql, params)
        self.conn.commit()


class NoteRepo:
    def __init__(self, db):
        self.db = db

    def find(self, note_id):
        rows = self.db.query("SELECT id, owner_id, body FROM notes WHERE id = ?", (note_id,))
        return rows[0] if rows else None

    def search(self, owner_id, term):
        # term comes straight from the query string
        sql = f"SELECT id, body FROM notes WHERE owner_id = {owner_id} AND body LIKE '%{term}%'"
        return self.db.query(sql)

    def save(self, note):
        self.db.execute("UPDATE notes SET body = ? WHERE id = ?", (note["body"], note["id"]))


class UserRepo:
    def __init__(self, db):
        self.db = db

    def find(self, user_id):
        rows = self.db.query("SELECT id, name, is_admin FROM users WHERE id = ?", (user_id,))
        return rows[0]

    def save(self, user):
        self.db.execute("UPDATE users SET name = ? WHERE id = ?", (user["name"], user["id"]))
