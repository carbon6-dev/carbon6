# decks-render: headless Chromium to print HTML decks, poppler to turn pages
# into images, python-pptx for PowerPoint, and open fonts metric-compatible
# with the common ones (Arial/Helvetica, Times, Palatino), plus Inter and Noto.
FROM debian:bookworm-slim
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      chromium poppler-utils python3 python3-pip python3-pil python3-lxml \
      fonts-liberation2 fonts-urw-base35 fonts-dejavu-core fonts-noto-core fonts-noto-color-emoji fonts-inter \
 && pip3 install --no-cache-dir --break-system-packages python-pptx==1.0.2 \
 && rm -rf /var/lib/apt/lists/* \
 && fc-cache -f
COPY render.py /opt/kit/render.py
