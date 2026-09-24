# Frigate's notebooks/YOLO_NAS_Pretrained_Export.ipynb, as a Dockerfile. The
# pretrained weights are Deci's, licensed for non-commercial use: local testing.
FROM python:3.10 AS build
RUN apt-get update && apt-get install --no-install-recommends -y libgl1 && rm -rf /var/lib/apt/lists/*
# requests: Colab, where the notebook runs, has it installed; super-gradients
# imports it without declaring it.
# albumentations 1.4 changed the classes super-gradients (unmaintained since 2024)
# registers at import; 1.3.1 is what it was written against.
RUN pip install -q "jedi>=0.16" requests git+https://github.com/Deci-AI/super-gradients.git "albumentations==1.3.1"
# What super-gradients asks for when export is called. It names NVIDIA's own index,
# which no longer resolves (NXDOMAIN); the package is on PyPI now.
RUN pip install -q onnx_graphsurgeon
RUN sed -i 's/sghub\.deci\.ai/d2gjn4b69gu75n.cloudfront.net/g; s/sg-hub-nv\.s3\.amazonaws\.com/d2gjn4b69gu75n.cloudfront.net/g' \
      /usr/local/lib/python3.10/site-packages/super_gradients/training/pretrained_models.py \
      /usr/local/lib/python3.10/site-packages/super_gradients/training/utils/checkpoint_utils.py
WORKDIR /out
RUN python3 -c "\
from super_gradients.common.object_names import Models; \
from super_gradients.conversion import DetectionOutputFormatMode; \
from super_gradients.training import models; \
m = models.get(Models.YOLO_NAS_S, pretrained_weights='coco'); \
m.export('yolo_nas_s.onnx', output_predictions_format=DetectionOutputFormatMode.FLAT_FORMAT, max_predictions_per_image=20, num_pre_nms_predictions=300, confidence_threshold=0.4, input_image_shape=(320, 320))"
FROM scratch
COPY --from=build /out/yolo_nas_s.onnx /yolo_nas_s.onnx
