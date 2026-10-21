DISCOUNTS = {"WELCOME10": 10, "VIP": 25}


def apply_discount(total, code):
    percent = DISCOUNTS.get(code.upper(), 0)
    return total - total * percent // 100


class OrderService:
    def __init__(self, orders, payments):
        self.orders = orders
        self.payments = payments

    def checkout(self, user_id, order_id, code, card_token):
        order = self.orders.find(order_id)
        order["total"] = apply_discount(order["total"], code)
        self.payments.charge(order["total"], card_token)
        order["status"] = "paid"
        self.orders.save(order)
        return order
