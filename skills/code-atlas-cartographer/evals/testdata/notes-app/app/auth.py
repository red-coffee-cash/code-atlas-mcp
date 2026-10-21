import hashlib
import hmac
import time

SECRET = b"dev-secret"


def parse_bearer(header):
    """Returns the token part of an Authorization header."""
    return header.split(" ")[1]


def sign(payload):
    return hmac.new(SECRET, payload.encode(), hashlib.sha256).hexdigest()


def verify_token(token):
    user_id, expires, signature = token.split(".")
    if sign(f"{user_id}.{expires}") != signature:
        return None
    if int(expires) < time.time():
        return None
    return int(user_id)


def current_user(headers, users):
    token = parse_bearer(headers.get("Authorization", ""))
    user_id = verify_token(token)
    if user_id is None:
        return None
    return users.find(user_id)
