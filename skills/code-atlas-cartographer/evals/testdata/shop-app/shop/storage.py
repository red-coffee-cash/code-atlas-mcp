import sqlite3


class Db:
    def __init__(self, path):
        self.conn = sqlite3.connect(path)

    def query(self, sql, params=()):
        return self.conn.execute(sql, params).fetchall()

    def execute(self, sql, params=()):
        self.conn.execute(sql, params)
        self.conn.commit()


class OrderRepo:
    def __init__(self, db):
        self.db = db

    def find(self, order_id):
        rows = self.db.query("SELECT id, user_id, total, status FROM orders WHERE id = ?", (order_id,))
        return rows[0] if rows else None

    def save(self, order):
        self.db.execute("UPDATE orders SET total = ?, status = ? WHERE id = ?",
                        (order["total"], order["status"], order["id"]))

    def all_for_export(self):
        return self.db.query("SELECT id, user_id, total, status FROM orders")


class RefundRepo:
    def __init__(self, db):
        self.db = db

    def find(self, refund_id):
        rows = self.db.query("SELECT id, order_id, amount FROM refunds WHERE id = ?", (refund_id,))
        return rows[0] if rows else None

    def save(self, refund):
        self.db.execute("INSERT INTO refunds (order_id, amount) VALUES (?, ?)",
                        (refund["order_id"], refund["amount"]))
