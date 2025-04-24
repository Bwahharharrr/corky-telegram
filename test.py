#!/usr/bin/env python3
import zmq
import json
import argparse
import time
from json import dumps
import binascii

def send_message(destination="telegram", chat_id=None, subscriber_list=None, text="Test message", 
                 status="ok", action="send_message", endpoint="tcp://127.0.0.1:6565", debug=False,
                 image_path=None):
    """
    Send a message to the Telegram bot via ZMQ
    
    Args:
        destination: The destination identifier
        chat_id: Optional chat ID to send message to a specific chat
        subscriber_list: Optional subscriber list name to send message to all subscribers in the list
        text: The message text to send
        status: Status field for the message
        action: Action to perform
        endpoint: ZMQ endpoint to connect to
        debug: Enable extra debugging information
        image_path: Optional path to an image file to send with the message
    """
    context = zmq.Context()
    
    # Create a DEALER socket to match the Rust app
    socket = context.socket(zmq.DEALER)
    
    # Set identity for this client - using setsockopt instead of set_identity
    sender_identity = b"test-client"
    socket.setsockopt(zmq.IDENTITY, sender_identity)
    
    # Connect to the endpoint
    print(f"Connecting to {endpoint}")
    socket.connect(endpoint)
    
    # Create the data payload
    data = {"text": text}
    if chat_id is not None:
        data["chat_id"] = chat_id
    if subscriber_list is not None:
        data["subscriber_list"] = subscriber_list
    if image_path is not None:
        data["image_path"] = image_path
    
    # Format in the structure as [status, action, data]
    message = [status, action, data]
    
    # Convert to JSON string
    msg = dumps(message, default=str)
    
    # Destination must be the identity of the recipient
    dest_bytes = str.encode(destination)
    
    if debug:
        print(f"Sender identity: {sender_identity}")
        print(f"Destination: {destination}")
        print(f"Message payload: {msg}")
    
    # Send multipart message: [destination, message]
    frames = [dest_bytes, str.encode(msg)]
    print(f"Sending multipart message to {destination}")
    
    if debug:
        print(f"Frame count: {len(frames)}")
        for i, frame in enumerate(frames):
            print(f"  Frame {i}: {frame}")
    
    socket.send_multipart(frames)
    
   
    # Close the socket
    socket.close()
    context.term()
    print("Message sent successfully")

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description='Send a message to Telegram bot via ZMQ')
    parser.add_argument('--destination', type=str, default='telegram',
                       help='Destination identifier')
    parser.add_argument('--chat-id', type=int, help='Chat ID to send message to')
    parser.add_argument('--list', type=str, help='Subscriber list name to send message to')
    parser.add_argument('--text', type=str, default='Test message from Python ZMQ client', 
                       help='Message text to send')
    parser.add_argument('--status', type=str, default='ok',
                       help='Status field for the message')
    parser.add_argument('--action', type=str, default='send_message',
                       help='Action to perform')
    parser.add_argument('--endpoint', type=str, default='tcp://127.0.0.1:6565',
                       help='ZMQ endpoint to connect to')
    parser.add_argument('--debug', action='store_true',
                       help='Enable extra debugging information')
    parser.add_argument('--image', type=str, help='Path to an image file to send with the message')
    
    args = parser.parse_args()
    
    # Ensure at least one destination is specified
    if args.chat_id is None and args.list is None:
        print("No destination specified. Message will be sent to the owner's chat ID.")
    
    send_message(
        destination=args.destination,
        chat_id=args.chat_id,
        subscriber_list=args.list,
        text=args.text,
        status=args.status,
        action=args.action,
        endpoint=args.endpoint,
        debug=args.debug,
        image_path=args.image
    )
