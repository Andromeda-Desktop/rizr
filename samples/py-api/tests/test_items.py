from app import add_item, list_items


def test_add_and_list():
    add_item("pen")
    assert "pen" in list_items()
