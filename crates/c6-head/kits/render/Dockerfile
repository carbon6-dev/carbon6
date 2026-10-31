# c6-render: carbon6's render kit (docs/docs.md §10). Headless Chromium to
# print HTML (documents and decks) to PDF, poppler to turn pages into
# pictures, pandoc for Word and OpenDocument text, python-pptx for
# PowerPoint, LibreOffice Impress to turn PowerPoint into OpenDocument
# presentations, and open fonts metric-compatible with the common ones.
FROM debian:bookworm-slim
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      chromium poppler-utils pandoc python3 python3-pip python3-pil python3-lxml \
      libreoffice-impress libreoffice-core \
      fonts-liberation2 fonts-urw-base35 fonts-dejavu-core fonts-noto-core fonts-noto-color-emoji fonts-inter \
 && pip3 install --no-cache-dir --break-system-packages python-pptx==1.0.2 \
 && rm -rf /var/lib/apt/lists/* \
 && fc-cache -f
COPY render.py /opt/kit/render.py
