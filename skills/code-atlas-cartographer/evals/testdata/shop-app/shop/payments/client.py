import requests


class PaymentClient:
    def __init__(self, api_key):
        self.session = requests.Session()
        self.session.headers["Authorization"] = "Bearer " + api_key

    def post(self, path, body):
        resp = self.session.post("https://pay.example.com" + path, json=body, timeout=10)
        resp.raise_for_status()
        return resp.json()

    def charge(self, amount, card_token):
        return self.post("/charges", {"amount": amount, "source": card_token})

    def refund(self, charge_id, amount):
        return self.post("/refunds", {"charge": charge_id, "amount": amount})
