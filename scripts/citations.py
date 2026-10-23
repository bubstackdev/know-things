"""Independent validation of the runtime's single/grouped source-label grammar."""


def citation_occurrences(text, source_count):
    labels = []
    tail = text
    while '[S' in tail:
        tail = tail.split('[S', 1)[1]
        if ']' not in tail:
            raise ValueError('Unclosed source label')
        group, tail = ('S'+tail).split(']', 1)
        for member in group.split(','):
            member = member.strip(' \t\n\r\v\f')
            raw = member[1:] if member.startswith('S') else ''
            if not raw or not raw.isascii() or not raw.isdecimal() or not 1 <= int(raw) <= source_count:
                raise ValueError('Unknown source label')
            labels.append(f'S{int(raw)}')
    return labels
