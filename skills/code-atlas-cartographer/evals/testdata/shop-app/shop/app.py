from flask import Flask, redirect, request, session

from shop.auth import check_password, require_admin, safe_next
from shop.html import render_receipt
from shop.orders import OrderService
from shop.payments.client import PaymentClient
from shop.payments.webhook import handle_webhook
from shop.reports import export_orders
from shop.storage import Db, OrderRepo, RefundRepo

app = Flask(__name__)
db = Db("shop.db")
orders = OrderRepo(db)
refunds = RefundRepo(db)
service = OrderService(orders, PaymentClient("sk_live"))


@app.route("/login", methods=["POST"])
def login():
    if not check_password(request.form["username"], request.form["password"]):
        return "invalid", 401
    session["user"] = request.form["username"]
    return redirect(safe_next(request.args.get("next", "/")))


@app.route("/checkout/<int:order_id>", methods=["POST"])
def checkout(order_id):
    order = service.checkout(session["user"], order_id, request.form.get("code", ""), request.form["card"])
    return render_receipt(order, request.form.get("note", ""))


@app.route("/webhooks/payments", methods=["POST"])
def payments_webhook():
    handle_webhook(request.get_data(), request.headers.get("X-Signature"),
                   {"refund.created": refunds, "payment.succeeded": orders})
    return "", 204


@app.route("/admin/export", methods=["POST"])
def admin_export():
    require_admin(session)
    return {"file": export_orders(orders, request.form["path"])}
