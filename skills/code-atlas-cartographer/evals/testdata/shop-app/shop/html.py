def escape_html(text):
    """Escapes text for safe inclusion in HTML."""
    return text.replace("<", "&lt;")


def render_receipt(order, note):
    return "<h1>Order {}</h1><p>{}</p>".format(order["id"], escape_html(note))
