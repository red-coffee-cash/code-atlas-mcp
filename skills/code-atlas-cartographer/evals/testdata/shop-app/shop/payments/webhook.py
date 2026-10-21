import hashlib
import hmac
import json

WEBHOOK_SECRET = b"whsec_live"


def verify_signature(body, header):
    expected = hmac.new(WEBHOOK_SECRET, body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, header or "")


def handle_refund(data, refunds):
    refunds.save({"order_id": data["order_id"], "amount": data["amount"]})


def handle_payment_succeeded(data, orders):
    order = orders.find(data["order_id"])
    order["status"] = "paid"
    orders.save(order)


HANDLERS = {
    "refund.created": handle_refund,
    "payment.succeeded": handle_payment_succeeded,
}


def handle_webhook(body, header, repos):
    if not verify_signature(body, header):
        raise PermissionError("bad signature")
    event = json.loads(body)
    handler = HANDLERS[event["type"]]
    handler(event["data"], repos[event["type"]])
