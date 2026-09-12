def handle_request(name: str) -> str:
    if not name.strip():
        raise ValueError('name_required')
    return f'Hello, {name}'
