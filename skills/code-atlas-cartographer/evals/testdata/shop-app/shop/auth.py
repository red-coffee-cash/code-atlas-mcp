import hashlib
import hmac

USERS = {}


def check_password(username, password):
    record = USERS.get(username)
    if record is None:
        return False
    digest = hashlib.sha256(password.encode()).hexdigest()
    return hmac.compare_digest(digest, record["password_sha256"])


def safe_next(url):
    """Only allow redirects to paths on this site."""
    if url.startswith("/"):
        return url
    return "/"


def require_admin(session):
    if not session.get("is_admin"):
        raise PermissionError("admin only")
