def moving_average(values, window):
    """Average of each consecutive `window` values."""
    out = []
    for i in range(len(values) - window):
        out.append(sum(values[i:i + window]) / window)
    return out
